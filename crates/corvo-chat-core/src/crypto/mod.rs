//! The core's encryption: sealing single values, and the account secrets.

mod secrets;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use zeroize::Zeroizing;

pub use secrets::{CopyKind, Secrets, lock_copy, unlock_copy};

const NONCE_LEN: usize = 24;

/// A purpose-specific key from a master key, so one key is never used for two jobs.
#[allow(dead_code)] // Used by the pins and the cache (later in this milestone).
pub(crate) fn subkey(key: &[u8; 32], label: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut out = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(None, key).expand(label, out.as_mut()).expect("32 bytes is a valid HKDF length");
    out
}

/// A stable, secret-keyed name for a value, so rows can be found by it without
/// storing the value itself: HMAC-SHA256(key, kind, 0, value). `kind` keeps
/// different sorts of value from ever sharing a tag.
#[allow(dead_code)] // Used by the pins and the cache (later in this milestone).
pub(crate) fn tag(key: &[u8; 32], kind: &[u8], value: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(kind);
    mac.update(&[0]);
    mac.update(value);
    mac.finalize().into_bytes().into()
}

/// Encrypts `plain` with a fresh random nonce. `aad` says where the value
/// belongs, so a value copied into another place won't open there. Output:
/// nonce, then ciphertext.
pub(crate) fn seal(key: &[u8; 32], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let ct = XChaCha20Poly1305::new(&(*key).into())
        .encrypt(&XNonce::from(nonce), Payload { msg: plain, aad })
        .expect("encrypting in memory can't fail");
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    out
}

/// Reverses [`seal`]. `None` if the key or `aad` is wrong or the bytes were changed.
pub(crate) fn open(key: &[u8; 32], aad: &[u8], sealed: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    if sealed.len() < NONCE_LEN {
        return None;
    }
    let (nonce, ct) = sealed.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce.try_into().ok()?;
    XChaCha20Poly1305::new(&(*key).into())
        .decrypt(&XNonce::from(nonce), Payload { msg: ct, aad })
        .ok()
        .map(Zeroizing::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7; 32];

    #[test]
    fn sealed_values_open_only_with_the_same_key_and_place() {
        let sealed = seal(&KEY, b"profile.name", b"abin");
        assert_eq!(open(&KEY, b"profile.name", &sealed).unwrap().as_slice(), b"abin");
        assert!(open(&[8; 32], b"profile.name", &sealed).is_none());
        assert!(open(&KEY, b"profile.seed", &sealed).is_none());
    }

    #[test]
    fn sealing_twice_gives_different_bytes() {
        assert_ne!(seal(&KEY, b"a", b"same"), seal(&KEY, b"a", b"same"));
    }

    #[test]
    fn changed_or_short_bytes_do_not_open() {
        let mut sealed = seal(&KEY, b"a", b"abin");
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert!(open(&KEY, b"a", &sealed).is_none());
        assert!(open(&KEY, b"a", &[0; 10]).is_none());
        assert!(open(&KEY, b"a", &[]).is_none());
    }

    #[test]
    fn ciphers_wipe_themselves() {
        fn wiped_on_drop<T: zeroize::ZeroizeOnDrop>() {}
        wiped_on_drop::<XChaCha20Poly1305>();
    }

    #[test]
    fn tags_are_stable_and_separated() {
        assert_eq!(tag(&KEY, b"peer", b"abc"), tag(&KEY, b"peer", b"abc"));
        assert_ne!(tag(&KEY, b"peer", b"abc"), tag(&KEY, b"msg", b"abc"));
        assert_ne!(tag(&KEY, b"peer", b"abc"), tag(&[8; 32], b"peer", b"abc"));
        assert_ne!(tag(&KEY, b"pe", b"erabc"), tag(&KEY, b"peer", b"abc"));
    }

    #[test]
    fn subkeys_differ_by_label() {
        assert_ne!(subkey(&KEY, b"one"), subkey(&KEY, b"two"));
    }
}
