//! Offline identity derivation and username validation. No proof of account ownership.
use crate::identity::Profile;
use md5::{Digest, Md5};
use uuid::Uuid;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
