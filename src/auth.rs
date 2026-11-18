//! Minecraft identity and fail-closed online-session verification.
//!
//! The protocol requires RSA PKCS#1 v1.5 and a signed SHA-1 server hash for
//! compatibility. Key generation/decryption are synchronous: callers should
//! run them on a blocking pool, and never expose cryptographic error details.
use std::{future::Future, io, pin::Pin, sync::Arc, time::Duration};

use md5::Md5;
use openssl::{
    encrypt::Decrypter,
    pkey::{PKey, Private},
    rsa::{Padding, Rsa},
};
use rand::{RngCore, rngs::OsRng};
use serde::Deserialize;
use sha1::{Digest, Sha1};
use subtle::ConstantTimeEq;
use uuid::Uuid;

const SESSION_URL: &str = "https://sessionserver.mojang.com/session/minecraft/hasJoined";
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_PROPERTIES: usize = 16;
const MAX_PROPERTY_NAME_CHARS: usize = 64;
const MAX_PROPERTY_VALUE_CHARS: usize = 32767;
const MAX_PROPERTY_SIGNATURE_CHARS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub uuid: Uuid,
    pub name: String,
    pub properties: Vec<ProfileProperty>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProfileProperty {
    pub name: String,
    pub value: String,
    pub signature: Option<String>,
}

pub fn valid_username(name: &str) -> bool {
    (1..=16).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Java's UUID.nameUUIDFromBytes("OfflinePlayer:" + name), including case.
/// The login handler must call `valid_username` before accepting the name.
pub fn offline_profile(name: &str) -> Profile {
    let mut hash = Md5::new();
    hash.update(b"OfflinePlayer:");
    hash.update(name.as_bytes());
    let mut bytes: [u8; 16] = hash.finalize().into();
    bytes[6] = (bytes[6] & 0x0f) | 0x30;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Profile {
        uuid: Uuid::from_bytes(bytes),
        name: name.to_owned(),
        properties: Vec::new(),
    }
}

/// Test seam for session verification, not a configurable authentication URL.
pub trait SessionVerifier: Send + Sync {
    fn verify<'a>(
        &'a self,
        username: &'a str,
        server_hash: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Profile>> + Send + 'a>>;
}

/// One ephemeral RSA key per server process; intentionally no Debug implementation.
pub struct AuthService {
    private_key: PKey<Private>,
    public_key: Vec<u8>,
    verifier: Arc<dyn SessionVerifier>,
}

impl AuthService {
    pub fn new() -> io::Result<Self> {
        let verifier = MojangVerifier::new()?;
        Self::with_verifier(Arc::new(verifier))
    }

    pub fn with_verifier(verifier: Arc<dyn SessionVerifier>) -> io::Result<Self> {
        // EVP default-provider PKCS1 implicit rejection was added in OpenSSL 3.2.
        if openssl::version::number() < 0x3020_0000 {
            return Err(io::Error::other(
                "login encryption requires OpenSSL 3.2 or later",
            ));
        }
        let private_key = Rsa::generate(1024)
            .and_then(PKey::from_rsa)
            .map_err(|_| io::Error::other("failed to generate login encryption key"))?;
        let public_key = private_key
            .public_key_to_der()
            .map_err(|_| io::Error::other("failed to encode login encryption key"))?;
        // A zero ciphertext cannot have valid PKCS1 padding. Supporting
        // providers return pseudorandom plaintext instead of a padding error.
        // Check the actual provider behavior as well as the reported version.
        let mut probe = [0; 128];
        decrypt_pkcs1(&private_key, &[0; 128], &mut probe)
            .map_err(|_| io::Error::other("login encryption requires RSA implicit rejection"))?;
        Ok(Self {
            private_key,
            public_key,
            verifier,
        })
    }

    pub fn public_key(&self) -> &[u8] {
        &self.public_key
    }

    pub fn challenge(&self) -> [u8; 4] {
        let mut challenge = [0u8; 4];
        OsRng.fill_bytes(&mut challenge);
        challenge
    }

    /// Uses EVP with OpenSSL's PKCS1 implicit rejection enabled.
    /// Do not use deprecated RSA_private_decrypt: its padding failure semantics
    /// do not provide the required oracle mitigation.
    pub fn decrypt_response(
        &self,
        encrypted_secret: &[u8],
        encrypted_token: &[u8],
        expected_token: &[u8; 4],
    ) -> io::Result<[u8; 16]> {
        // Public ciphertext lengths may be rejected before expensive work.
        if encrypted_secret.len() != 128 || encrypted_token.len() != 128 {
            return Err(invalid_encryption());
        }
        let mut secret = [0; 128];
        let mut token = [0; 128];
        // Always run both operations. Buffer sizes and comparison work do not
        // depend on decrypted lengths; each decrypt has implicit rejection.
        let secret_len = decrypt_pkcs1(&self.private_key, encrypted_secret, &mut secret);
        let token_len = decrypt_pkcs1(&self.private_key, encrypted_token, &mut token);
        let valid = token[..4].ct_eq(expected_token)
            & u8::from(matches!(secret_len, Ok(16))).ct_eq(&1)
            & u8::from(matches!(token_len, Ok(4))).ct_eq(&1);
        if !bool::from(valid) {
            return Err(invalid_encryption());
        }
        let mut shared_secret = [0; 16];
        shared_secret.copy_from_slice(&secret[..16]);
        Ok(shared_secret)
    }

    /// No offline fallback: transport, status, body and identity errors all deny login.
    pub async fn authenticate(&self, username: &str, secret: &[u8; 16]) -> io::Result<Profile> {
        if !valid_username(username) {
            return Err(invalid_profile());
        }
        let hash = server_hash(secret, &self.public_key);
        let profile = tokio::time::timeout(AUTH_TIMEOUT, self.verifier.verify(username, &hash))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "session verification timed out")
            })??;
        validate_profile(username, &profile)?;
        Ok(profile)
    }
}

struct MojangVerifier {
    client: reqwest::Client,
}

impl MojangVerifier {
    fn new() -> io::Result<Self> {
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

fn validate_profile(username: &str, profile: &Profile) -> io::Result<()> {
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

fn server_hash(secret: &[u8; 16], public_key: &[u8]) -> String {
    let mut sha1 = Sha1::new();
    // The server ID is the empty string, contributing zero bytes.
    sha1.update(secret);
    sha1.update(public_key);
    signed_hex(sha1.finalize().into())
}

/// Java BigInteger(byte[]).toString(16), not ordinary SHA-1 hex encoding.
fn signed_hex(mut bytes: [u8; 20]) -> String {
    let negative = bytes[0] & 0x80 != 0;
    if negative {
        let mut carry = true;
        for byte in bytes.iter_mut().rev() {
            *byte = !*byte;
            if carry {
                let (next, overflow) = byte.overflowing_add(1);
                *byte = next;
                carry = overflow;
            }
        }
    }
    let mut value = String::with_capacity(41);
    if negative {
        value.push('-');
    }
    if let Some(first) = bytes.iter().position(|byte| *byte != 0) {
        use std::fmt::Write;
        write!(&mut value, "{:x}", bytes[first]).expect("String formatting cannot fail");
        for byte in &bytes[first + 1..] {
            write!(&mut value, "{byte:02x}").expect("String formatting cannot fail");
        }
    } else {
        value.push('0');
    }
    value
}

fn decrypt_pkcs1(
    key: &PKey<Private>,
    ciphertext: &[u8],
    plaintext: &mut [u8; 128],
) -> io::Result<usize> {
    let mut decrypter = Decrypter::new(key).map_err(|_| invalid_encryption())?;
    decrypter
        .set_rsa_padding(Padding::PKCS1)
        .map_err(|_| invalid_encryption())?;
    // EVP_PKEY_decrypt, not RSA_private_decrypt. Never disable implicit rejection.
    let result = decrypter.decrypt(ciphertext, plaintext);
    // Drain the thread-local error stack unconditionally; never expose details.
    drop(openssl::error::ErrorStack::get());
    result.map_err(|_| invalid_encryption())
}

fn invalid_encryption() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "invalid encryption response",
    )
}

fn invalid_profile() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "invalid authenticated profile",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::encrypt::Encrypter;
    use std::sync::{Mutex, OnceLock};

    struct MockVerifier {
        result: Option<Profile>,
        calls: Mutex<Vec<(String, String)>>,
    }

    impl SessionVerifier for MockVerifier {
        fn verify<'a>(
            &'a self,
            username: &'a str,
            server_hash: &'a str,
        ) -> Pin<Box<dyn Future<Output = io::Result<Profile>> + Send + 'a>> {
            self.calls
                .lock()
                .unwrap()
                .push((username.into(), server_hash.into()));
            Box::pin(async move {
                self.result
                    .clone()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            })
        }
    }

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

    fn service() -> &'static AuthService {
        static SERVICE: OnceLock<AuthService> = OnceLock::new();
        SERVICE.get_or_init(|| {
            AuthService::with_verifier(Arc::new(MockVerifier {
                result: Some(profile("Notch")),
                calls: Mutex::new(Vec::new()),
            }))
            .unwrap()
        })
    }

    #[test]
    fn minecraft_signed_sha1_vectors() {
        for (input, expected) in [
            ("Notch", "4ed1f46bbe04bc756bcb17c0c7ce3e4632f06a48"),
            ("jeb_", "-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1"),
            ("simon", "88e16a1019277b15d58faf0541e11910eb756f6"),
        ] {
            assert_eq!(signed_hex(Sha1::digest(input.as_bytes()).into()), expected);
        }
        assert_eq!(signed_hex([0; 20]), "0");
        assert_eq!(signed_hex([0xff; 20]), "-1");
    }

    #[test]
    fn server_hash_uses_empty_id_then_secret_then_spki_bytes() {
        // Independently calculated with hashlib.sha1(secret + public_key).
        assert_eq!(
            server_hash(&[7; 16], b"public key DER"),
            "6ac96778171b0f1a29c201e6a4adea2012a572c3"
        );
    }

    #[test]
    fn offline_uuid_matches_java_and_preserves_case() {
        assert_eq!(
            offline_profile("Notch").uuid.to_string(),
            "b50ad385-829d-3141-a216-7e7d7539ba7f"
        );
        assert_eq!(
            offline_profile("Steve").uuid.to_string(),
            "5627dd98-e6be-3c21-b8a8-e92344183641"
        );
        assert_ne!(offline_profile("Steve").uuid, offline_profile("steve").uuid);
        assert!(offline_profile("Steve").properties.is_empty());
    }

    #[test]
    fn usernames_are_ascii_and_bounded() {
        for name in ["A", "jeb_", "0123456789abcdef"] {
            assert!(valid_username(name));
        }
        for name in ["", "0123456789abcdefg", "has space", "世界", "a-b", "a\n"] {
            assert!(!valid_username(name));
        }
    }

    #[test]
    fn active_openssl_backend_supports_implicit_rejection() {
        println!("crypto backend: {}", openssl::version::version());
        assert!(openssl::version::number() >= 0x3020_0000);
        let mut plaintext = [0; 128];
        // Ciphertext zero decrypts to an all-zero, invalidly padded block.
        // Success therefore proves this backend uses implicit rejection.
        let length = decrypt_pkcs1(&service().private_key, &[0; 128], &mut plaintext).unwrap();
        assert!(length <= 117);
        assert!(openssl::error::ErrorStack::get().errors().is_empty());
    }

    #[test]
    fn rsa_spki_roundtrip_and_uniform_rejection() {
        let service = service();
        let public_key = PKey::public_key_from_der(service.public_key()).unwrap();
        assert_eq!(public_key.bits(), 1024);
        assert!(Rsa::public_key_from_der_pkcs1(service.public_key()).is_err());
        let encrypt = |plaintext: &[u8]| {
            let mut encrypter = Encrypter::new(&public_key).unwrap();
            encrypter.set_rsa_padding(Padding::PKCS1).unwrap();
            let mut output = vec![0; encrypter.encrypt_len(plaintext).unwrap()];
            let len = encrypter.encrypt(plaintext, &mut output).unwrap();
            output.truncate(len);
            output
        };
        let secret = [7; 16];
        let token = service.challenge();
        let encrypted_secret = encrypt(&secret);
        let encrypted_token = encrypt(&token);
        assert_eq!(
            service
                .decrypt_response(&encrypted_secret, &encrypted_token, &token)
                .unwrap(),
            secret
        );
        let mut other_token = token;
        other_token[0] ^= 1;
        let short_secret = encrypt(&[0; 15]);
        let long_token = encrypt(&[0; 5]);
        let cases = [
            (&encrypted_secret[..], &encrypted_token[..], &other_token),
            (&short_secret[..], &encrypted_token[..], &token),
            (&encrypted_secret[..], &long_token[..], &token),
            (&[0; 128][..], &encrypted_token[..], &token),
            (&encrypted_secret[..127], &encrypted_token[..], &token),
            (&encrypted_secret[..], &[0; 128][..], &token),
            (&[0; 128][..], &[0; 128][..], &token),
            (&[0xff; 128][..], &encrypted_token[..], &token),
            (&encrypted_secret[..], &[0xff; 128][..], &token),
        ];
        for (secret, encrypted_token, token) in cases {
            let error = service
                .decrypt_response(secret, encrypted_token, token)
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(error.to_string(), "invalid encryption response");
        }
    }

    #[tokio::test]
    async fn authentication_uses_hash_and_server_identity() {
        let verifier = Arc::new(MockVerifier {
            result: Some(profile("Notch")),
            calls: Mutex::new(Vec::new()),
        });
        let service = AuthService::with_verifier(verifier.clone()).unwrap();
        let secret = [9; 16];
        let actual = service.authenticate("notch", &secret).await.unwrap();
        assert_eq!(actual.name, "Notch");
        assert_eq!(actual, profile("Notch"));
        assert_eq!(
            *verifier.calls.lock().unwrap(),
            vec![("notch".into(), server_hash(&secret, service.public_key()))]
        );
        assert!(service.authenticate("OtherPlayer", &secret).await.is_err());
        assert!(service.authenticate("bad name", &secret).await.is_err());
        assert_eq!(verifier.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn verifier_errors_never_fall_back_offline() {
        let service = AuthService::with_verifier(Arc::new(MockVerifier {
            result: None,
            calls: Mutex::new(Vec::new()),
        }))
        .unwrap();
        assert!(service.authenticate("Notch", &[0; 16]).await.is_err());
    }

    #[tokio::test]
    async fn unresponsive_verifier_is_bounded_by_authentication_deadline() {
        struct PendingVerifier;
        impl SessionVerifier for PendingVerifier {
            fn verify<'a>(
                &'a self,
                _username: &'a str,
                _server_hash: &'a str,
            ) -> Pin<Box<dyn Future<Output = io::Result<Profile>> + Send + 'a>> {
                Box::pin(std::future::pending())
            }
        }
        let service = AuthService::with_verifier(Arc::new(PendingVerifier)).unwrap();
        let error = service.authenticate("Notch", &[0; 16]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
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

    #[tokio::test]
    async fn invalid_mock_identity_is_checked_by_authenticate() {
        let mut candidate = profile("Notch");
        candidate.uuid = Uuid::nil();
        let service = AuthService::with_verifier(Arc::new(MockVerifier {
            result: Some(candidate),
            calls: Mutex::new(Vec::new()),
        }))
        .unwrap();
        assert!(service.authenticate("Notch", &[0; 16]).await.is_err());
    }
}
