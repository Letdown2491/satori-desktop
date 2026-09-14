//! Signing — port of `src/data/signer.ts` semantics, minus everything the
//! browser forced. Two modes (DESIGN.md D2):
//!
//! - `Bunker`: NIP-46 remote signing via the nostr-connect client. The app
//!   never sees the user's key; only its own ephemeral client keypair does.
//! - `Local`: opt-in nsec, sealed in the secret store.

use std::str::FromStr;
use std::sync::Arc;

use nostr::prelude::*;
use nostr_connect::prelude::*;

use crate::error::{LoginError, Result};

#[derive(Clone)]
pub enum Signer {
    Bunker(Arc<NostrConnect>),
    Local(Keys),
}

impl Signer {
    pub async fn public_key(&self) -> Result<PublicKey> {
        match self {
            Self::Bunker(c) => c
                .get_public_key_async()
                .await
                .map_err(|e| LoginError::Connect(e.to_string())),
            Self::Local(k) => Ok(k.public_key()),
        }
    }

    pub async fn sign_event(&self, unsigned: UnsignedEvent) -> Result<Event> {
        match self {
            Self::Bunker(c) => c
                .sign_event_async(unsigned)
                .await
                .map_err(|e| LoginError::Connect(e.to_string())),
            Self::Local(k) => {
                k.sign_event(unsigned).map_err(|e| LoginError::Connect(e.to_string()))
            }
        }
    }

    /// NIP-44 encrypt to self (private lists — NIP-51).
    pub async fn nip44_encrypt_self(&self, me: &PublicKey, plaintext: &str) -> Result<String> {
        match self {
            Self::Bunker(c) => c
                .nip44_encrypt_async(me, plaintext)
                .await
                .map_err(|e| LoginError::Connect(e.to_string())),
            Self::Local(k) => nostr::nips::nip44::encrypt(
                k.secret_key(),
                me,
                plaintext,
                nostr::nips::nip44::Version::V2,
            )
            .map_err(|e| LoginError::Connect(e.to_string())),
        }
    }

    /// NIP-44 decrypt from self.
    pub async fn nip44_decrypt_self(&self, me: &PublicKey, payload: &str) -> Result<String> {
        match self {
            Self::Bunker(c) => c
                .nip44_decrypt_async(me, payload)
                .await
                .map_err(|e| LoginError::Connect(e.to_string())),
            Self::Local(k) => nostr::nips::nip44::decrypt(
                k.secret_key(),
                me,
                payload,
            )
            .map_err(|e| LoginError::Connect(e.to_string())),
        }
    }
}

/// Parse an nsec (NIP-19 bech32) or hex secret key.
pub fn parse_local_secret(secret: &str) -> Result<Keys> {
    Keys::parse(secret.trim()).map_err(|_| LoginError::InvalidNsec)
}

/// Rebuild client keys from the persisted transport secret (hex).
pub fn client_keys_from_secret(secret: &str) -> Result<Keys> {
    let sk = SecretKey::from_str(secret.trim()).map_err(|_| LoginError::Resume("bad client secret".into()))?;
    Ok(Keys::new(sk))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Any 32-byte value; this is a throwaway test key, never used for identity.
    const TEST_HEX: &str = "e1d4a5b33a9d2f0c8b7e6f5a4d3c2b1a0918273645f4e3d2c1b0a99887766554";

    #[test]
    fn parses_hex_secret() {
        let keys = parse_local_secret(TEST_HEX).expect("hex should parse");
        assert_eq!(keys.public_key().to_string().len(), 64);
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            parse_local_secret("not a key"),
            Err(LoginError::InvalidNsec)
        ));
    }
}
