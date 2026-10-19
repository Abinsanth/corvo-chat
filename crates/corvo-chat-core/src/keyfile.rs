//! The local key file: the account secrets and session token locked under the
//! password, so the app can start without the network. Pure bytes and JSON;
//! reading and writing the file is the native layer's job.

use argon2::{Algorithm, Argon2, Block, Params, Version};
use corvo_chat_proto::{B64, Blob, frames::Device};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::crypto::{normalize_password, open, seal};
use crate::{Error, Secrets, SignedIn, Strength};

const VERSION: u32 = 1;
const AAD: &[u8] = b"corvo-chat local v1";
/// Secrets plaintext, then the session token.
const PLAIN_LEN: usize = 97 + 32;

/// The most a key file may ask Argon2 for, so a crafted one can't make
/// unlocking use gigabytes of memory or run for hours.
const MAX_M_KIB: u32 = 1024 * 1024;
const MAX_T: u32 = 16;
const MAX_P: u32 = 8;

/// How hard Argon2id works, saved in the file so unlocking uses what it was locked with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Kdf {
    m_kib: u32,
    t: u32,
    p: u32,
}

impl Kdf {
    fn of(s: Strength) -> Kdf {
        match s {
            Strength::Normal => Kdf { m_kib: 64 * 1024, t: 3, p: 1 },
            #[cfg(feature = "insecure-test-kdf")]
            Strength::FastInsecure => Kdf { m_kib: 8, t: 1, p: 1 },
        }
    }

    /// The Argon2 settings, if they're within the caps and Argon2 accepts them.
    fn params(self) -> Option<Params> {
        if self.m_kib > MAX_M_KIB || self.t > MAX_T || self.p > MAX_P {
            return None;
        }
        Params::new(self.m_kib, self.t, self.p, Some(32)).ok()
    }
}

/// The password stretched into a key. `None` if the settings are unusable.
fn stretch(password: &str, salt: &[u8; 16], kdf: Kdf) -> Option<Zeroizing<[u8; 32]>> {
    let params = kdf.params()?;
    let password = normalize_password(password);
    let mut key = Zeroizing::new([0u8; 32]);
    // Argon2's working memory holds what the key is made from: ours, so it's wiped.
    let mut memory = Zeroizing::new(vec![Block::default(); params.block_count()]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into_with_memory(password.as_bytes(), salt, key.as_mut(), memory.as_mut_slice())
        .ok()?;
    Some(key)
}

/// A locked key file: who it's for, and the secrets sealed under the password.
#[derive(Clone, Debug)]
pub struct LocalKey {
    pub server: String,
    pub username: String,
    pub device_id: [u8; 16],
    kdf: Kdf,
    salt: [u8; 16],
    sealed: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    version: u32,
    server: String,
    username: String,
    device_id: B64<16>,
    kdf: Kdf,
    salt: B64<16>,
    sealed: Blob,
}

impl LocalKey {
    /// Locks the secrets and the session token under `password`.
    pub fn lock(
        server: &str,
        username: &str,
        device_id: [u8; 16],
        password: &str,
        secrets: &Secrets,
        token: &[u8; 32],
        s: Strength,
    ) -> LocalKey {
        let kdf = Kdf::of(s);
        let mut salt = [0u8; 16];
        OsRng.fill_bytes(&mut salt);
        let key = stretch(password, &salt, kdf).expect("the built-in settings are valid");
        let mut plain = Zeroizing::new(Vec::with_capacity(PLAIN_LEN));
        plain.extend_from_slice(secrets.to_bytes().as_ref());
        plain.extend_from_slice(token);
        LocalKey {
            server: server.to_string(),
            username: username.to_string(),
            device_id,
            kdf,
            salt,
            sealed: seal(&key, AAD, &plain),
        }
    }

    /// The secrets and session token. `WrongPassword` if the password doesn't
    /// open the file, `DamagedKeyFile` if it opens to something unreadable.
    pub fn unlock(&self, password: &str) -> Result<(Secrets, Zeroizing<[u8; 32]>), Error> {
        let key = stretch(password, &self.salt, self.kdf).ok_or(Error::DamagedKeyFile)?;
        let plain = open(&key, AAD, &self.sealed).ok_or(Error::WrongPassword)?;
        if plain.len() != PLAIN_LEN {
            return Err(Error::DamagedKeyFile);
        }
        let secrets = Secrets::from_bytes(&plain[..97]).ok_or(Error::DamagedKeyFile)?;
        let mut token = Zeroizing::new([0u8; 32]);
        token.copy_from_slice(&plain[97..]);
        Ok((secrets, token))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(&Wire {
            version: VERSION,
            server: self.server.clone(),
            username: self.username.clone(),
            device_id: B64(self.device_id),
            kdf: self.kdf,
            salt: B64(self.salt),
            sealed: Blob(self.sealed.clone()),
        })
        .expect("a key file always serializes")
    }

    /// `DamagedKeyFile` for bad JSON or base64, wrong lengths, an unknown
    /// version, or Argon2 settings that are too big or that Argon2 refuses.
    pub fn from_json(json: &str) -> Result<LocalKey, Error> {
        let wire: Wire = serde_json::from_str(json).map_err(|_| Error::DamagedKeyFile)?;
        if wire.version != VERSION || wire.kdf.params().is_none() {
            return Err(Error::DamagedKeyFile);
        }
        Ok(LocalKey {
            server: wire.server,
            username: wire.username,
            device_id: wire.device_id.0,
            kdf: wire.kdf,
            salt: wire.salt.0,
            sealed: wire.sealed.0,
        })
    }
}

/// What locking a new key file takes, besides the password and secrets.
#[cfg_attr(not(feature = "native"), allow(dead_code))]
pub(crate) struct KeyFile {
    pub server: String,
    pub username: String,
    pub device_id: [u8; 16],
    pub token: Zeroizing<[u8; 32]>,
    pub strength: Strength,
}

#[cfg_attr(not(feature = "native"), allow(dead_code))]
impl KeyFile {
    pub(crate) fn lock(&self, password: &str, secrets: &Secrets) -> LocalKey {
        LocalKey::lock(&self.server, &self.username, self.device_id, password, secrets, &self.token, self.strength)
    }
}

/// The key file for an account just signed in (signed up, signed in, or
/// recovered), locked under `password`. Writes nothing: the app saves
/// `to_json()` readable by its owner only.
pub fn first_start(url: &str, signed_in: &SignedIn, password: &str, device: &Device, s: Strength) -> LocalKey {
    LocalKey::lock(url, &signed_in.account.username, device.id.0, password, &signed_in.secrets, &signed_in.token, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locked(password: &str) -> (LocalKey, Secrets) {
        let secrets = Secrets::generate();
        let key =
            LocalKey::lock("wss://chat.example", "abin", [9; 16], password, &secrets, &[5; 32], Strength::FastInsecure);
        (key, secrets)
    }

    #[test]
    fn key_file_round_trips_and_refuses_a_wrong_password() {
        let (key, secrets) = locked("pw");
        let back = LocalKey::from_json(&key.to_json()).unwrap();
        assert_eq!(
            (back.server.as_str(), back.username.as_str(), back.device_id),
            ("wss://chat.example", "abin", [9; 16])
        );
        let (got, token) = back.unlock("pw").unwrap();
        assert_eq!(got.public_keys(), secrets.public_keys());
        assert_eq!(got.data_key(), secrets.data_key());
        assert_eq!(*token, [5; 32]);
        assert!(matches!(back.unlock("nope"), Err(Error::WrongPassword)));
    }

    #[test]
    fn the_password_is_normalized() {
        let (key, _) = locked("e\u{301}");
        assert!(key.unlock("\u{e9}").is_ok());
    }

    #[test]
    fn locking_twice_gives_different_files() {
        let secrets = Secrets::generate();
        let lock = || LocalKey::lock("s", "u", [1; 16], "pw", &secrets, &[2; 32], Strength::FastInsecure).to_json();
        assert_ne!(lock(), lock());
    }

    fn with_json(key: &LocalKey, edit: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut value: serde_json::Value = serde_json::from_str(&key.to_json()).unwrap();
        edit(&mut value);
        value.to_string()
    }

    #[test]
    fn key_file_with_huge_kdf_settings_is_damaged() {
        let (key, _) = locked("pw");
        for (field, big) in [("m_kib", 4u64 << 20), ("t", 17), ("p", 9)] {
            let json = with_json(&key, |v| v["kdf"][field] = big.into());
            assert!(matches!(LocalKey::from_json(&json), Err(Error::DamagedKeyFile)), "{field}");
        }
    }

    #[test]
    fn key_file_with_settings_below_argons_minimum_is_damaged() {
        let (key, _) = locked("pw");
        for (field, small) in [("m_kib", 0u64), ("m_kib", 7), ("t", 0), ("p", 0)] {
            let json = with_json(&key, |v| v["kdf"][field] = small.into());
            assert!(matches!(LocalKey::from_json(&json), Err(Error::DamagedKeyFile)), "{field}");
        }
    }

    #[test]
    fn damaged_files_are_refused() {
        let (key, _) = locked("pw");
        let edits: [(&str, serde_json::Value); 5] = [
            ("version", 2.into()),
            ("salt", "AAAA".into()),
            ("device_id", "not base64!".into()),
            ("sealed", "***".into()),
            ("username", 7.into()),
        ];
        for (field, bad) in edits {
            let json = with_json(&key, |v| v[field] = bad);
            assert!(matches!(LocalKey::from_json(&json), Err(Error::DamagedKeyFile)), "{field}");
        }
        assert!(matches!(LocalKey::from_json("{"), Err(Error::DamagedKeyFile)));
    }

    #[test]
    fn a_changed_file_does_not_unlock() {
        let (key, _) = locked("pw");
        let mut other = key.clone();
        other.sealed[30] ^= 1;
        assert!(matches!(other.unlock("pw"), Err(Error::WrongPassword)));
    }
}
