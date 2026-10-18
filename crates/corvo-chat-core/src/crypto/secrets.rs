//! The account secrets: the signing key, the exchange key and the data key.
//! They never leave the client except locked inside a copy (one under the
//! password, one under the recovery code).

use corvo_chat_proto::{B64, Blob, PublicKeys, exchange_key_proof};
use ed25519_dalek::{Signer, SigningKey};
use rand_core::{OsRng, RngCore};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use super::{open, seal};

/// The layout version, the first byte of the secrets' plaintext.
const VERSION: u8 = 1;
/// Version, signing seed, exchange secret, data key.
const LEN: usize = 1 + 32 + 32 + 32;

/// Which wrap key a copy is locked under. Part of the AAD, so a password copy
/// never opens as a recovery copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyKind {
    Password,
    Recovery,
}

impl CopyKind {
    fn aad(self) -> &'static [u8] {
        match self {
            CopyKind::Password => b"corvo-chat copy v1 password",
            CopyKind::Recovery => b"corvo-chat copy v1 recovery",
        }
    }
}

/// Everything that makes the account yours. Wiped when dropped.
pub struct Secrets {
    signing: SigningKey,
    exchange: StaticSecret,
    data_key: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secrets(…)")
    }
}

impl Secrets {
    /// Fresh random secrets for a new account.
    pub fn generate() -> Secrets {
        let mut data_key = Zeroizing::new([0u8; 32]);
        OsRng.fill_bytes(data_key.as_mut());
        Secrets { signing: SigningKey::generate(&mut OsRng), exchange: StaticSecret::random_from_rng(OsRng), data_key }
    }

    /// The public half, with the signing key's signature over the exchange key.
    pub fn public_keys(&self) -> PublicKeys {
        let exchange = PublicKey::from(&self.exchange).to_bytes();
        PublicKeys {
            signing: B64(self.signing.verifying_key().to_bytes()),
            exchange: B64(exchange),
            signature: self.sign(&exchange_key_proof(&exchange)),
        }
    }

    /// An Ed25519 signature over `msg`.
    pub fn sign(&self, msg: &[u8]) -> B64<64> {
        B64(self.signing.sign(msg).to_bytes())
    }

    pub fn exchange_secret(&self) -> &StaticSecret {
        &self.exchange
    }

    pub fn data_key(&self) -> &[u8; 32] {
        &self.data_key
    }

    /// The plaintext a copy locks: version, signing seed, exchange secret, data key.
    pub fn to_bytes(&self) -> Zeroizing<[u8; LEN]> {
        let mut out = Zeroizing::new([0u8; LEN]);
        out[0] = VERSION;
        out[1..33].copy_from_slice(&self.signing.to_bytes());
        out[33..65].copy_from_slice(&self.exchange.to_bytes());
        out[65..].copy_from_slice(self.data_key.as_ref());
        out
    }

    /// Reverses [`Secrets::to_bytes`]. `None` for the wrong length or version.
    pub fn from_bytes(bytes: &[u8]) -> Option<Secrets> {
        let bytes: &[u8; LEN] = bytes.try_into().ok()?;
        if bytes[0] != VERSION {
            return None;
        }
        let seed = Zeroizing::new(<[u8; 32]>::try_from(&bytes[1..33]).ok()?);
        let exchange = Zeroizing::new(<[u8; 32]>::try_from(&bytes[33..65]).ok()?);
        Some(Secrets {
            signing: SigningKey::from_bytes(&seed),
            exchange: StaticSecret::from(*exchange),
            data_key: Zeroizing::new(<[u8; 32]>::try_from(&bytes[65..]).ok()?),
        })
    }
}

/// The secrets sealed under `wrap_key`, ready to store on the server.
pub fn lock_copy(secrets: &Secrets, wrap_key: &[u8; 32], kind: CopyKind) -> Blob {
    Blob(seal(wrap_key, kind.aad(), secrets.to_bytes().as_ref()))
}

/// Reverses [`lock_copy`]. `None` if the key or kind is wrong or the copy was changed.
pub fn unlock_copy(copy: &Blob, wrap_key: &[u8; 32], kind: CopyKind) -> Option<Secrets> {
    Secrets::from_bytes(&open(wrap_key, kind.aad(), &copy.0)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_keys_pass_the_servers_check() {
        assert!(Secrets::generate().public_keys().verify());
    }

    #[test]
    fn copy_opens_only_with_its_key_and_kind() {
        let s = Secrets::generate();
        let copy = lock_copy(&s, &[1; 32], CopyKind::Password);
        assert_eq!(copy.0.len(), 137);
        let back = unlock_copy(&copy, &[1; 32], CopyKind::Password).unwrap();
        assert_eq!(back.public_keys(), s.public_keys());
        assert_eq!(back.data_key(), s.data_key());
        assert!(unlock_copy(&copy, &[2; 32], CopyKind::Password).is_none());
        assert!(unlock_copy(&copy, &[1; 32], CopyKind::Recovery).is_none());
    }

    #[test]
    fn bytes_with_another_version_are_refused() {
        let s = Secrets::generate();
        let mut bytes = s.to_bytes();
        assert!(Secrets::from_bytes(bytes.as_ref()).is_some());
        bytes[0] = 2;
        assert!(Secrets::from_bytes(bytes.as_ref()).is_none());
        assert!(Secrets::from_bytes(&bytes[..96]).is_none());
    }

    #[test]
    fn debug_hides_the_secrets() {
        assert_eq!(format!("{:?}", Secrets::generate()), "Secrets(…)");
    }
}
