//! Login state machine and identity policy, independent of sockets.
use super::{Context, Outcome};
use crate::{
    auth::{AuthService, Profile, offline_profile, valid_username},
    config::Config,
    protocol::{
        State, invalid,
        packets::{
            configuration::ConfigurationDisconnect,
            login::{
                EncryptionRequest, EncryptionResponse, LoginAcknowledged, LoginDisconnect,
                LoginStart, LoginSuccess,
            },
        },
    },
};
use std::{io, sync::Arc};
use tokio::sync::Semaphore;

pub struct LoginService {
    auth: Option<Arc<AuthService>>,
    crypto_slots: Arc<Semaphore>,
}

impl LoginService {
    pub fn offline() -> Self {
        Self {
            auth: None,
            crypto_slots: Arc::new(Semaphore::new(16)),
        }
    }

    pub fn online(auth: Arc<AuthService>) -> Self {
        Self {
            auth: Some(auth),
            crypto_slots: Arc::new(Semaphore::new(16)),
        }
    }

    pub async fn from_config(config: &Config) -> io::Result<Self> {
        if config.offline_mode {
            return Ok(Self::offline());
        }
        let auth = tokio::task::spawn_blocking(AuthService::new)
            .await
            .map_err(|_| io::Error::other("login key generation task failed"))??;
        Ok(Self::online(Arc::new(auth)))
    }
}

#[derive(Debug, Default)]
pub enum LoginPhase {
    #[default]
    AwaitStart,
    AwaitEncryption {
        name: String,
        token: [u8; 4],
    },
    AwaitAcknowledged,
    Finished,
}

pub fn disconnect(reason: &str) -> Outcome {
    Outcome {
        reply: Some(
            LoginDisconnect {
                reason: serde_json::json!({"text": reason}).to_string(),
            }
            .into(),
        ),
        close: true,
        encryption: None,
    }
}

fn success(ctx: &mut Context<'_>, profile: Profile) -> Outcome {
    ctx.session.profile = Some(profile.clone());
    ctx.session.login = LoginPhase::AwaitAcknowledged;
    Outcome {
        reply: Some(LoginSuccess { profile }.into()),
        close: false,
        encryption: None,
    }
}

pub async fn handle_start(ctx: &mut Context<'_>, packet: LoginStart) -> io::Result<Outcome> {
    if !matches!(ctx.session.login, LoginPhase::AwaitStart) {
        return Err(invalid("unexpected Login Start"));
    }
    // Consume the phase even on failure: challenges/identities are single-use.
    ctx.session.login = LoginPhase::Finished;
    if !valid_username(&packet.name) {
        return Ok(disconnect(
            "ZCraft: invalid username (use 1-16 ASCII letters, digits or underscores).",
        ));
    }
    if let Some(auth) = &ctx.server.login.auth {
        let token = auth.challenge();
        ctx.session.login = LoginPhase::AwaitEncryption {
            name: packet.name,
            token,
        };
        Ok(Outcome {
            reply: Some(
                EncryptionRequest {
                    public_key: auth.public_key().to_vec(),
                    verify_token: token,
                }
                .into(),
            ),
            close: false,
            encryption: None,
        })
    } else {
        // Never use claimed_uuid, even when a client sends a plausible online UUID.
        Ok(success(ctx, offline_profile(&packet.name)))
    }
}

pub async fn handle_encryption_response(
    ctx: &mut Context<'_>,
    packet: EncryptionResponse,
) -> io::Result<Outcome> {
    let phase = std::mem::replace(&mut ctx.session.login, LoginPhase::Finished);
    let LoginPhase::AwaitEncryption { name, token } = phase else {
        return Err(invalid("unexpected Encryption Response"));
    };
    let auth = ctx
        .server
        .login
        .auth
        .as_ref()
        .ok_or_else(|| invalid("encryption response in offline mode"))?
        .clone();
    let permit = ctx
        .server
        .login
        .crypto_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| io::Error::other("login cryptography is busy"))?;
    let decryptor = auth.clone();
    let secret = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        decryptor.decrypt_response(&packet.shared_secret, &packet.verify_token, &token)
    })
    .await
    .map_err(|_| io::Error::other("login cryptography task failed"))??;

    // The client has already enabled encryption. Every response from this point,
    // including a failed Mojang lookup, must be encrypted.
    let mut outcome = match auth.authenticate(&name, &secret).await {
        Ok(profile) => success(ctx, profile),
        Err(error) => {
            tracing::warn!(%error, "online login verification failed");
            disconnect("ZCraft: failed to verify your session. Please sign in again and retry.")
        }
    };
    outcome.encryption = Some(secret);
    Ok(outcome)
}

pub async fn handle_acknowledged(
    ctx: &mut Context<'_>,
    _packet: LoginAcknowledged,
) -> io::Result<Outcome> {
    if !matches!(ctx.session.login, LoginPhase::AwaitAcknowledged) || ctx.session.profile.is_none()
    {
        return Err(invalid("unexpected Login Acknowledged"));
    }
    ctx.session.login = LoginPhase::Finished;
    ctx.session.state = State::Configuration;
    Ok(Outcome {
        reply: Some(ConfigurationDisconnect.into()),
        close: true,
        encryption: None,
    })
}
