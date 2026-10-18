//! Pinned keys: the public keys first seen for a number, kept sealed so the
//! server can't swap them unnoticed, and the safety code two people compare.

use corvo_chat_proto::{Blob, Number, PublicKeys};
use sha2::{Digest, Sha512};

use super::{Secrets, open, seal, subkey};

const LABEL: &[u8] = b"corvo-chat pin v1";

fn aad(number: Number) -> Vec<u8> {
    [LABEL, number.digits().as_bytes()].concat()
}

/// `keys` sealed under the data key, for storing on the server. The number is
/// in the AAD, so a pin for one person won't open as another's.
pub fn seal_pin(secrets: &Secrets, number: Number, keys: &PublicKeys) -> Blob {
    let key = subkey(secrets.data_key(), LABEL);
    let json = serde_json::to_vec(keys).expect("public keys always serialize");
    Blob(seal(&key, &aad(number), &json))
}

/// Reverses [`seal_pin`]. `None` if the pin isn't yours, isn't for this number
/// or was changed.
pub fn open_pin(secrets: &Secrets, number: Number, pin: &Blob) -> Option<PublicKeys> {
    let key = subkey(secrets.data_key(), LABEL);
    serde_json::from_slice(&open(&key, &aad(number), &pin.0)?).ok()
}

/// The 12 groups of 5 digits two people read out to each other. Both sides get
/// the same code: the signing keys are hashed smaller first.
pub fn safety_code(a: &PublicKeys, b: &PublicKeys) -> String {
    let (lo, hi) = if a.signing.0 <= b.signing.0 { (a, b) } else { (b, a) };
    let hash = Sha512::new().chain_update(lo.signing.0).chain_update(hi.signing.0).finalize();
    let groups: Vec<String> = (0..12)
        .map(|i| {
            let mut word = [0u8; 8];
            word[3..].copy_from_slice(&hash[5 * i..5 * i + 5]);
            format!("{:05}", u64::from_be_bytes(word) % 100_000)
        })
        .collect();
    groups.join(" ")
}

#[cfg(test)]
mod tests {
    use corvo_chat_proto::B64;

    use super::*;

    fn number(body: u32) -> Number {
        Number::from_body(body).unwrap()
    }

    #[test]
    fn safety_code_is_the_same_from_both_sides() {
        let (a, b) = (Secrets::generate().public_keys(), Secrets::generate().public_keys());
        let code = safety_code(&a, &b);
        assert_eq!(code, safety_code(&b, &a));
        assert_eq!(code.split(' ').count(), 12);
        assert!(code.split(' ').all(|g| g.len() == 5 && g.bytes().all(|c| c.is_ascii_digit())));
    }

    #[test]
    fn safety_code_matches_a_hand_computed_group() {
        let keys = |byte: u8| PublicKeys { signing: B64([byte; 32]), exchange: B64([0; 32]), signature: B64([0; 64]) };
        let (low, high) = (keys(1), keys(2));
        let mut input = vec![1u8; 32];
        input.extend_from_slice(&[2u8; 32]);
        let hash = Sha512::digest(&input);
        // Group 2 reads bytes 10..15.
        let n = hash[10..15].iter().fold(0u64, |n, &b| n * 256 + u64::from(b));
        let expected = format!("{:05}", n % 100_000);
        let code = safety_code(&high, &low);
        assert_eq!(code.split(' ').nth(2).unwrap(), expected);
        assert_eq!(code, safety_code(&low, &high));
    }

    #[test]
    fn different_keys_give_different_codes() {
        let (a, b, c) =
            (Secrets::generate().public_keys(), Secrets::generate().public_keys(), Secrets::generate().public_keys());
        assert_ne!(safety_code(&a, &b), safety_code(&a, &c));
    }

    #[test]
    fn pin_opens_only_for_its_number_and_account() {
        let me = Secrets::generate();
        let their = Secrets::generate().public_keys();
        let pin = seal_pin(&me, number(111_111_111), &their);
        assert_eq!(open_pin(&me, number(111_111_111), &pin), Some(their));
        assert!(open_pin(&me, number(222_222_222), &pin).is_none());
        assert!(open_pin(&Secrets::generate(), number(111_111_111), &pin).is_none());
    }

    #[test]
    fn pin_aad_is_pinned() {
        let me = Secrets::generate();
        let their = Secrets::generate().public_keys();
        let n = number(111_111_111);
        let key = subkey(me.data_key(), b"corvo-chat pin v1");
        let aad = [b"corvo-chat pin v1".as_slice(), n.digits().as_bytes()].concat();
        let pin = Blob(seal(&key, &aad, &serde_json::to_vec(&their).unwrap()));
        assert_eq!(open_pin(&me, n, &pin), Some(their));
    }
}
