//! Authentication facade: composes cryptography and fail-closed session verification.
//! Shared identity types live in crate::identity, not in authentication.
mod crypto;
mod mojang;
mod offline;

pub use mojang::SessionVerifier;
pub use offline::{offline_profile, valid_username};

use crate::identity::Profile;
use crypto::{LoginCrypto, server_hash};
use mojang::{MojangVerifier, invalid_profile, validate_profile};
use std::{io, sync::Arc, time::Duration};

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

pub struct AuthService {
    crypto: LoginCrypto,
    verifier: Arc<dyn SessionVerifier>,
}

impl AuthService {
    pub fn new() -> io::Result<Self> {
        Self::with_verifier(Arc::new(MojangVerifier::new()?))
    }

    pub fn with_verifier(verifier: Arc<dyn SessionVerifier>) -> io::Result<Self> {
        Ok(Self {
            crypto: LoginCrypto::new()?,
            verifier,
        })
    }

    pub fn public_key(&self) -> &[u8] {
        self.crypto.public_key()
    }

    pub fn challenge(&self) -> [u8; 4] {
        self.crypto.challenge()
    }

    /// Synchronous RSA operation; run on a bounded blocking pool.
    pub fn decrypt_response(
        &self,
        encrypted_secret: &[u8],
        encrypted_token: &[u8],
        expected_token: &[u8; 4],
    ) -> io::Result<[u8; 16]> {
        self.crypto
            .decrypt_response(encrypted_secret, encrypted_token, expected_token)
    }

    /// No offline fallback: transport, status, body and identity errors all deny login.
    pub async fn authenticate(&self, username: &str, secret: &[u8; 16]) -> io::Result<Profile> {
        if !valid_username(username) {
            return Err(invalid_profile());
        }
        let hash = server_hash(secret, self.crypto.public_key());
        let profile = tokio::time::timeout(AUTH_TIMEOUT, self.verifier.verify(username, &hash))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "session verification timed out")
            })??;
        validate_profile(username, &profile)?;
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::ProfileProperty;
    use std::{future::Future, pin::Pin, sync::Mutex};
    use uuid::Uuid;

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
