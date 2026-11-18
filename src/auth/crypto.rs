//! Minecraft login cryptography. AES stream encryption lives in transport.rs.
//!
//! RSA PKCS1v1.5 and signed SHA-1 are protocol compatibility requirements.
//! Key generation and decryption are synchronous; callers must use a blocking pool.
use openssl::{
    encrypt::Decrypter,
    pkey::{PKey, Private},
    rsa::{Padding, Rsa},
};
use rand::{RngCore, rngs::OsRng};
use sha1::{Digest, Sha1};
use std::io;
use subtle::ConstantTimeEq;

/// One ephemeral key per process. Never expose key material through Debug.
pub(super) struct LoginCrypto {
    private_key: PKey<Private>,
    public_key: Vec<u8>,
}

impl LoginCrypto {
    pub(super) fn new() -> io::Result<Self> {
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
        })
    }

    pub(super) fn public_key(&self) -> &[u8] {
        &self.public_key
    }

    pub(super) fn challenge(&self) -> [u8; 4] {
        let mut challenge = [0u8; 4];
        OsRng.fill_bytes(&mut challenge);
        challenge
    }

    /// Uses EVP with OpenSSL's PKCS1 implicit rejection enabled.
    /// Do not use deprecated RSA_private_decrypt: its padding failure semantics
    /// do not provide the required oracle mitigation.
    pub(super) fn decrypt_response(
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
}
pub(super) fn server_hash(secret: &[u8; 16], public_key: &[u8]) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::encrypt::Encrypter;
    use std::sync::OnceLock;

    fn service() -> &'static LoginCrypto {
        static CRYPTO: OnceLock<LoginCrypto> = OnceLock::new();
        CRYPTO.get_or_init(|| LoginCrypto::new().unwrap())
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
        let token = [1, 2, 3, 4];
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
}
