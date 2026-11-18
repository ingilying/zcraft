//! Fixed-endpoint Mojang session verification with bounded, validated responses.
use super::{AUTH_TIMEOUT, offline::valid_username};
use crate::identity::{Profile, ProfileProperty};
use serde::Deserialize;
use std::{future::Future, io, pin::Pin, time::Duration};
use uuid::Uuid;

const SESSION_URL: &str = "https://sessionserver.mojang.com/session/minecraft/hasJoined";
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_PROPERTIES: usize = 16;
const MAX_PROPERTY_NAME_CHARS: usize = 64;
const MAX_PROPERTY_VALUE_CHARS: usize = 32767;
const MAX_PROPERTY_SIGNATURE_CHARS: usize = 4096;

/// Test seam for session verification, not a configurable authentication URL.
pub trait SessionVerifier: Send + Sync {
    fn verify<'a>(
        &'a self,
        username: &'a str,
        server_hash: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Profile>> + Send + 'a>>;
}

pub(super) struct MojangVerifier {
    client: reqwest::Client,
}

impl MojangVerifier {
    pub(super) fn new() -> io::Result<Self> {
        let client = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(AUTH_TIMEOUT)
            .build()
            .map_err(|_| io::Error::other("failed to initialize session verification client"))?;
        Ok(Self { client })
    }

    async fn request(&self, username: &str, server_hash: &str) -> io::Result<Profile> {
        let mut response = self
            .client
            .get(SESSION_URL)
            .query(&[("username", username), ("serverId", server_hash)])
            .send()
            .await
            .map_err(|_| io::Error::other("session verification request failed"))?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "session verification rejected login",
            ));
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
        {
            return Err(invalid_profile());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| io::Error::other("failed to read session verification response"))?
        {
            if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
                return Err(invalid_profile());
            }
            body.extend_from_slice(&chunk);
        }
        parse_profile(&body, username)
    }
}

impl SessionVerifier for MojangVerifier {
    fn verify<'a>(
        &'a self,
        username: &'a str,
        server_hash: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Profile>> + Send + 'a>> {
        Box::pin(self.request(username, server_hash))
    }
}

#[derive(Deserialize)]
struct SessionProfile {
    id: String,
    name: String,
    properties: Vec<ProfileProperty>,
}

fn parse_profile(body: &[u8], username: &str) -> io::Result<Profile> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(invalid_profile());
    }
    let response: SessionProfile = serde_json::from_slice(body).map_err(|_| invalid_profile())?;
    // Mojang's hasJoined endpoint specifies an undashed 32-character profile ID.
    if response.id.len() != 32 || !response.id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid_profile());
    }
    let profile = Profile {
        uuid: Uuid::parse_str(&response.id).map_err(|_| invalid_profile())?,
        name: response.name,
        properties: response.properties,
    };
    validate_profile(username, &profile)?;
    Ok(profile)
}

pub(super) fn validate_profile(username: &str, profile: &Profile) -> io::Result<()> {
    if !valid_username(username)
        || !valid_username(&profile.name)
        || !profile.name.eq_ignore_ascii_case(username)
        || profile.uuid.is_nil()
        || profile.properties.len() > MAX_PROPERTIES
    {
        return Err(invalid_profile());
    }
    // Check mock verifiers as strictly as remote responses. Bound aggregate
    // profile fields too, so a test seam cannot bypass the network body limit.
    let mut total = profile.name.len();
    for property in &profile.properties {
        if property.name.is_empty()
            || property.name.encode_utf16().count() > MAX_PROPERTY_NAME_CHARS
            || property.value.encode_utf16().count() > MAX_PROPERTY_VALUE_CHARS
            || property.signature.as_ref().is_some_and(|signature| {
                signature.encode_utf16().count() > MAX_PROPERTY_SIGNATURE_CHARS
            })
        {
            return Err(invalid_profile());
        }
        total = total
            .saturating_add(property.name.len())
            .saturating_add(property.value.len())
            .saturating_add(property.signature.as_ref().map_or(0, String::len));
        if total > MAX_RESPONSE_BYTES {
            return Err(invalid_profile());
        }
    }
    Ok(())
}

pub(super) fn invalid_profile() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "invalid authenticated profile",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str) -> Profile {
        Profile {
            uuid: Uuid::parse_str("069a79f444e94726a5befca90e38aaf5").unwrap(),
            name: name.into(),
            properties: vec![ProfileProperty {
                name: "textures".into(),
                value: "dGV4dHVyZXM=".into(),
                signature: Some("c2lnbmF0dXJl".into()),
            }],
        }
    }

    #[test]
    fn validates_remote_and_mock_profile_fields() {
        let mut invalid = Vec::new();
        let mut nil = profile("Notch");
        nil.uuid = Uuid::nil();
        invalid.push(nil);
        let mut too_many = profile("Notch");
        too_many.properties = vec![too_many.properties[0].clone(); MAX_PROPERTIES + 1];
        invalid.push(too_many);
        for (field, length) in [("name", 65), ("value", 32768), ("signature", 4097)] {
            let mut large = profile("Notch");
            let property = &mut large.properties[0];
            match field {
                "name" => property.name = "x".repeat(length),
                "value" => property.value = "x".repeat(length),
                _ => property.signature = Some("x".repeat(length)),
            }
            invalid.push(large);
        }
        let mut empty_name = profile("Notch");
        empty_name.properties[0].name.clear();
        invalid.push(empty_name);
        let mut too_many_bytes = profile("Notch");
        too_many_bytes.properties[0].value = "世".repeat(32767);
        invalid.push(too_many_bytes);
        for candidate in invalid {
            assert!(validate_profile("Notch", &candidate).is_err());
        }
    }

    #[test]
    fn malformed_or_incomplete_json_profiles_are_rejected() {
        let valid = br#"{"id":"069a79f444e94726a5befca90e38aaf5","name":"Notch","properties":[]}"#;
        assert_eq!(parse_profile(valid, "notch").unwrap().name, "Notch");
        for body in [
            "{}",
            "null",
            "[]",
            "not json",
            r#"{"id":"069a79f444e94726a5befca90e38aaf5","name":"Notch"}"#,
            r#"{"id":"00000000000000000000000000000000","name":"Notch","properties":[]}"#,
            r#"{"id":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"Notch","properties":[]}"#,
            r#"{"id":"069a79f444e94726a5befca90e38aaf5","name":"Other","properties":[]}"#,
            r#"{"id":"069a79f444e94726a5befca90e38aaf5","name":"Notch","properties":[{"name":"textures"}]}"#,
        ] {
            assert!(parse_profile(body.as_bytes(), "Notch").is_err(), "{body}");
        }
        assert!(parse_profile(&vec![b' '; MAX_RESPONSE_BYTES + 1], "Notch").is_err());
    }
}
