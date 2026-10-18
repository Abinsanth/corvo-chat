//! Chat keys sealed to each member, the creator's chat signature, message ids
//! and message encryption.

use corvo_chat_proto::chat::{ChatInfo, Message, NewChat};
use corvo_chat_proto::{BODY_MAX, Blob, Body, Number, PublicKeys, chat_proof};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use super::secrets::Secrets;
use super::{open, seal};
use crate::Error;

/// The most text one message holds, so its ciphertext stays under 64 KiB.
const TEXT_MAX: usize = 60_000;
const SEAL_LABEL: &[u8] = b"corvo-chat seal v1";
/// Ephemeral public key, nonce, 32-byte key and its tag.
const SEALED_LEN: usize = 32 + 24 + 32 + 16;
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The 32-byte key of one chat. Wiped when dropped.
pub struct ChatKey(Zeroizing<[u8; 32]>);

impl std::fmt::Debug for ChatKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatKey(…)")
    }
}

impl ChatKey {
    pub fn generate() -> ChatKey {
        let mut key = Zeroizing::new([0u8; 32]);
        OsRng.fill_bytes(key.as_mut());
        ChatKey(key)
    }
}

fn seal_aad(chat_id: &[u8; 16], to: Number) -> Vec<u8> {
    [SEAL_LABEL, chat_id, to.digits().as_bytes()].concat()
}

fn seal_wrap_key(shared: &[u8; 32], ephemeral: &[u8; 32], recipient: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    let salt = [ephemeral.as_slice(), recipient.as_slice()].concat();
    let mut out = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(&salt), shared).expand(SEAL_LABEL, out.as_mut()).expect("32 bytes is a valid HKDF length");
    out
}

/// Seals the chat key to `to`'s exchange key: ephemeral public key, nonce, ciphertext.
pub fn seal_key(key: &ChatKey, chat_id: &[u8; 16], to: Number, their: &PublicKeys) -> Blob {
    let ephemeral = StaticSecret::random_from_rng(OsRng);
    let ephemeral_pub = PublicKey::from(&ephemeral).to_bytes();
    let shared = ephemeral.diffie_hellman(&PublicKey::from(their.exchange.0));
    let wrap = seal_wrap_key(shared.as_bytes(), &ephemeral_pub, &their.exchange.0);
    let mut out = ephemeral_pub.to_vec();
    out.extend_from_slice(&seal(&wrap, &seal_aad(chat_id, to), key.0.as_ref()));
    Blob(out)
}

/// Opens the copy sealed to `me`. `None` if it isn't for me or was changed.
pub fn open_key(sealed: &Blob, chat_id: &[u8; 16], me: Number, secrets: &Secrets) -> Option<ChatKey> {
    if sealed.0.len() != SEALED_LEN {
        return None;
    }
    let (ephemeral, rest) = sealed.0.split_at(32);
    let ephemeral: [u8; 32] = ephemeral.try_into().ok()?;
    let secret = secrets.exchange_secret();
    let shared = secret.diffie_hellman(&PublicKey::from(ephemeral));
    let wrap = seal_wrap_key(shared.as_bytes(), &ephemeral, &PublicKey::from(secret).to_bytes());
    let plain = open(&wrap, &seal_aad(chat_id, me), rest)?;
    if plain.len() != 32 {
        return None;
    }
    let mut key = Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&plain);
    Some(ChatKey(key))
}

/// A fresh chat from `me` to `to`: the key, and what the first message carries.
pub fn new_chat(
    secrets: &Secrets,
    me: Number,
    chat_id: &[u8; 16],
    to: Number,
    their: &PublicKeys,
) -> (ChatKey, NewChat) {
    let key = ChatKey::generate();
    let my_key = seal_key(&key, chat_id, me, &secrets.public_keys());
    let their_key = seal_key(&key, chat_id, to, their);
    let signature = secrets.sign(&chat_proof(chat_id, me, &my_key.0, to, &their_key.0));
    (key, NewChat { number: to, my_key, their_key, signature })
}

/// Checks the creator's signature and opens my copy. The creator is the first
/// key's number; any failure is [`Error::BadChat`].
pub fn chat_key(
    info: &ChatInfo,
    me: Number,
    my_keys: &PublicKeys,
    their_keys: &PublicKeys,
    secrets: &Secrets,
) -> Result<ChatKey, Error> {
    let [first, second] = info.keys.as_slice() else { return Err(Error::BadChat) };
    let creator_keys = if first.number == me {
        my_keys
    } else if second.number == me {
        their_keys
    } else {
        return Err(Error::BadChat);
    };
    if first.number == second.number {
        return Err(Error::BadChat);
    }
    let proof = chat_proof(&info.chat_id.0, first.number, &first.key.0, second.number, &second.key.0);
    if !creator_keys.verifies(&proof, &info.signature) {
        return Err(Error::BadChat);
    }
    let (mine, other) = if first.number == me { (first, second) } else { (second, first) };
    if other.number != info.with {
        return Err(Error::BadChat);
    }
    open_key(&mine.key, &info.chat_id.0, me, secrets).ok_or(Error::BadChat)
}

/// A message id: a ULID's 16 bytes, 48 bits of milliseconds then 80 random bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsgId(pub [u8; 16]);

impl MsgId {
    pub fn new(now_ms: u64) -> MsgId {
        let mut bytes = [0u8; 16];
        OsRng.fill_bytes(&mut bytes[6..]);
        bytes[..6].copy_from_slice(&now_ms.to_be_bytes()[2..]);
        MsgId(bytes)
    }

    /// The 26-character Crockford base32 form.
    pub fn ulid(&self) -> String {
        let n = u128::from_be_bytes(self.0);
        (0..26).rev().map(|i| CROCKFORD[((n >> (5 * i)) & 31) as usize] as char).collect()
    }

    /// Reads a ULID in either case. `None` if it isn't 26 valid characters
    /// within 128 bits.
    pub fn from_ulid(s: &str) -> Option<MsgId> {
        if s.len() != 26 {
            return None;
        }
        let mut n = 0u128;
        for (i, c) in s.bytes().enumerate() {
            let v = CROCKFORD.iter().position(|d| *d == c.to_ascii_uppercase())? as u128;
            if i == 0 && v > 7 {
                return None;
            }
            n = (n << 5) | v;
        }
        Some(MsgId(n.to_be_bytes()))
    }
}

/// What a decrypted message holds.
#[derive(Debug, PartialEq, Eq)]
pub struct Plain {
    pub text: String,
    pub sent_at_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct Payload {
    kind: String,
    msg_id: String,
    sent_at: u64,
    body: String,
}

/// Where a message belongs: chat id, sender's number, message id.
fn message_aad(chat_id: &[u8; 16], sender: Number, id: &MsgId) -> Vec<u8> {
    [chat_id.as_slice(), sender.digits().as_bytes(), &id.0].concat()
}

/// Encrypts a text message. [`Error::TooLong`] if the text is over 60 000 bytes
/// or the ciphertext would pass the wire cap.
pub fn encrypt_message(
    key: &ChatKey,
    chat_id: &[u8; 16],
    sender: Number,
    id: &MsgId,
    sent_at_ms: u64,
    text: &str,
) -> Result<Body, Error> {
    if text.len() > TEXT_MAX {
        return Err(Error::TooLong);
    }
    let payload = Zeroizing::new(
        serde_json::to_vec(&Payload { kind: "text".into(), msg_id: id.ulid(), sent_at: sent_at_ms, body: text.into() })
            .expect("a payload always serializes"),
    );
    let sealed = seal(&key.0, &message_aad(chat_id, sender, id), &payload);
    if sealed.len() > BODY_MAX {
        return Err(Error::TooLong);
    }
    Ok(Body(sealed))
}

/// Opens a message from the server. Any failure is [`Error::Undecryptable`].
pub fn decrypt_message(key: &ChatKey, msg: &Message) -> Result<Plain, Error> {
    let id = MsgId(msg.msg_id.0);
    let plain = open(&key.0, &message_aad(&msg.chat_id.0, msg.from, &id), &msg.body.0).ok_or(Error::Undecryptable)?;
    let payload: Payload = serde_json::from_slice(&plain).map_err(|_| Error::Undecryptable)?;
    if payload.kind != "text" || MsgId::from_ulid(&payload.msg_id) != Some(id) {
        return Err(Error::Undecryptable);
    }
    Ok(Plain { text: payload.body, sent_at_ms: payload.sent_at })
}

#[cfg(test)]
mod tests {
    use corvo_chat_proto::chat::{ChatInfo, Message, NewChat, SealedKey};
    use corvo_chat_proto::{B64, Number, chat_proof};

    use super::*;
    use crate::Error;

    fn number(body: u32) -> Number {
        Number::from_body(body).unwrap()
    }

    /// What the server would send both members for a chat abin created.
    fn info(chat_id: &[u8; 16], abin: Number, rahul: Number, new: &NewChat) -> ChatInfo {
        ChatInfo {
            chat_id: B64(*chat_id),
            with: rahul,
            keys: vec![
                SealedKey { number: abin, key: new.my_key.clone() },
                SealedKey { number: rahul, key: new.their_key.clone() },
            ],
            signature: new.signature.clone(),
            last_seq: 0,
            read_through: 0,
            cleared_through: 0,
            marks: None,
        }
    }

    /// The same chat as the viewer whose peer is `with` gets it.
    fn seen_by(info: &ChatInfo, with: Number) -> ChatInfo {
        ChatInfo { with, ..info.clone() }
    }

    #[test]
    fn both_members_open_the_chat_and_the_server_check_passes() {
        let (abin, rahul) = (Secrets::generate(), Secrets::generate());
        let (an, rn) = (number(111_111_111), number(222_222_222));
        let id = [9u8; 16];
        let (key, new) = new_chat(&abin, an, &id, rn, &rahul.public_keys());
        let info = info(&id, an, rn, &new);
        let (ak, rk) = (abin.public_keys(), rahul.public_keys());

        let mine = chat_key(&info, an, &ak, &rk, &abin).unwrap();
        let theirs = chat_key(&seen_by(&info, an), rn, &rk, &ak, &rahul).unwrap();
        assert_eq!(mine.0[..], key.0[..]);
        assert_eq!(theirs.0[..], key.0[..]);

        // The server's check in open_chat.
        let proof = chat_proof(&id, an, &new.my_key.0, rn, &new.their_key.0);
        assert!(ak.verifies(&proof, &new.signature));
        assert_eq!(new.number, rn);
        assert_eq!(new.my_key.0.len(), 104);
    }

    #[test]
    fn a_chat_signed_by_someone_else_is_refused() {
        let (abin, rahul, eve) = (Secrets::generate(), Secrets::generate(), Secrets::generate());
        let (an, rn) = (number(111_111_111), number(222_222_222));
        let id = [9u8; 16];
        let (_, new) = new_chat(&abin, an, &id, rn, &rahul.public_keys());
        let info = seen_by(&info(&id, an, rn, &new), an);
        // Rahul thinks the creator's keys are Eve's.
        let err = chat_key(&info, rn, &rahul.public_keys(), &eve.public_keys(), &rahul);
        assert!(matches!(err, Err(Error::BadChat)));
        // A chat with one copy, or with no copy for me, is refused too.
        let mut one = info.clone();
        one.keys.pop();
        assert!(matches!(chat_key(&one, rn, &rahul.public_keys(), &abin.public_keys(), &rahul), Err(Error::BadChat)));
        let third = number(333_333_333);
        assert!(matches!(chat_key(&info, third, &eve.public_keys(), &abin.public_keys(), &eve), Err(Error::BadChat)));
    }

    fn message(id: &[u8; 16], from: Number, mid: &MsgId, body: corvo_chat_proto::Body) -> Message {
        Message { chat_id: B64(*id), seq: 1, msg_id: B64(mid.0), from, body, sent_at: 5 }
    }

    #[test]
    fn message_round_trips_and_tampering_fails() {
        let key = ChatKey::generate();
        let (id, from) = ([4u8; 16], number(111_111_111));
        let mid = MsgId::new(1_700_000_000_000);
        let body = encrypt_message(&key, &id, from, &mid, 1_700_000_000_000, "hello \"there\" é").unwrap();
        let plain = decrypt_message(&key, &message(&id, from, &mid, body.clone())).unwrap();
        assert_eq!(plain.text, "hello \"there\" é");
        assert_eq!(plain.sent_at_ms, 1_700_000_000_000);

        let bad = |m: Message| matches!(decrypt_message(&key, &m), Err(Error::Undecryptable));
        assert!(bad(message(&id, from, &MsgId::new(1), body.clone())));
        assert!(bad(message(&id, number(222_222_222), &mid, body.clone())));
        assert!(bad(message(&[5u8; 16], from, &mid, body.clone())));
        let mut flipped = body.clone();
        flipped.0[30] ^= 1;
        assert!(bad(message(&id, from, &mid, flipped)));
        assert!(matches!(
            decrypt_message(&ChatKey::generate(), &message(&id, from, &mid, body)),
            Err(Error::Undecryptable)
        ));
    }

    #[test]
    fn a_payload_with_another_kind_or_msg_id_is_undecryptable() {
        let key = ChatKey::generate();
        let (id, from) = ([4u8; 16], number(111_111_111));
        let mid = MsgId::new(7);
        let aad = message_aad(&id, from, &mid);
        let other = MsgId::new(8).ulid();
        for json in [
            format!(r#"{{"kind":"image","msg_id":"{}","sent_at":1,"body":"x"}}"#, mid.ulid()),
            format!(r#"{{"kind":"text","msg_id":"{other}","sent_at":1,"body":"x"}}"#),
        ] {
            let body = corvo_chat_proto::Body(crate::crypto::seal(&key.0, &aad, json.as_bytes()));
            assert!(matches!(decrypt_message(&key, &message(&id, from, &mid, body)), Err(Error::Undecryptable)));
        }
    }

    #[test]
    fn the_payload_is_the_agreed_json() {
        let key = ChatKey::generate();
        let (id, from, mid) = ([4u8; 16], number(111_111_111), MsgId::new(7));
        let body = encrypt_message(&key, &id, from, &mid, 42, "hi").unwrap();
        let aad = message_aad(&id, from, &mid);
        let plain = crate::crypto::open(&key.0, &aad, &body.0).unwrap();
        let want = format!(r#"{{"kind":"text","msg_id":"{}","sent_at":42,"body":"hi"}}"#, mid.ulid());
        assert_eq!(String::from_utf8(plain.to_vec()).unwrap(), want);
        assert_eq!(aad.len(), 16 + 10 + 16);
    }

    #[test]
    fn msg_ids_sort_by_time_and_round_trip() {
        let a = MsgId::new(1_000);
        let b = MsgId::new(2_000);
        assert!(a.0 < b.0);
        assert_eq!(MsgId::from_ulid(&a.ulid()).unwrap().0, a.0);
        assert_eq!(MsgId::from_ulid(&a.ulid().to_lowercase()).unwrap().0, a.0);
        assert_eq!(a.ulid().len(), 26);
        assert!(a.ulid() < b.ulid());
        assert!(MsgId::from_ulid("8ZZZZZZZZZZZZZZZZZZZZZZZZZ").is_none());
        assert!(MsgId::from_ulid("0000000000000000000000000").is_none());
        assert!(MsgId::from_ulid("0000000000000000000000000U").is_none());
        assert_eq!(MsgId([0; 16]).ulid(), "00000000000000000000000000");
        assert_eq!(MsgId([255; 16]).ulid(), "7ZZZZZZZZZZZZZZZZZZZZZZZZZ");
    }

    #[test]
    fn text_over_the_cap_is_refused() {
        let key = ChatKey::generate();
        let (id, from, mid) = ([4u8; 16], number(111_111_111), MsgId::new(1));
        let over = "a".repeat(60_001);
        assert!(matches!(encrypt_message(&key, &id, from, &mid, 1, &over), Err(Error::TooLong)));
        let at = "a".repeat(60_000);
        assert!(encrypt_message(&key, &id, from, &mid, 1, &at).unwrap().0.len() <= corvo_chat_proto::BODY_MAX);
        // Control characters grow when escaped; the body still never passes the wire cap.
        let escaped = "\u{1}".repeat(60_000);
        assert!(matches!(encrypt_message(&key, &id, from, &mid, 1, &escaped), Err(Error::TooLong)));
    }

    #[test]
    fn a_chat_labelled_with_someone_else_is_refused() {
        let (abin, rahul) = (Secrets::generate(), Secrets::generate());
        let (an, rn, third) = (number(111_111_111), number(222_222_222), number(333_333_333));
        let id = [9u8; 16];
        let (_, new) = new_chat(&abin, an, &id, rn, &rahul.public_keys());
        let mut info = info(&id, an, rn, &new);
        let (ak, rk) = (abin.public_keys(), rahul.public_keys());
        info.with = third;
        // Abin created it and is the viewer, then rahul is the viewer.
        assert!(matches!(chat_key(&info, an, &ak, &rk, &abin), Err(Error::BadChat)));
        assert!(matches!(chat_key(&info, rn, &rk, &ak, &rahul), Err(Error::BadChat)));
    }

    #[test]
    fn other_chat_failures_are_refused() {
        let (abin, rahul) = (Secrets::generate(), Secrets::generate());
        let (an, rn) = (number(111_111_111), number(222_222_222));
        let id = [9u8; 16];
        let (_, new) = new_chat(&abin, an, &id, rn, &rahul.public_keys());
        let good = seen_by(&info(&id, an, rn, &new), an);
        let (ak, rk) = (abin.public_keys(), rahul.public_keys());
        let refused = |i: &ChatInfo| matches!(chat_key(i, rn, &rk, &ak, &rahul), Err(Error::BadChat));
        assert!(!refused(&good));

        let mut swapped = good.clone();
        swapped.keys.reverse();
        assert!(refused(&swapped));

        let mut flipped = good.clone();
        flipped.signature.0[0] ^= 1;
        assert!(refused(&flipped));

        // A valid sealed copy of another chat key in place of rahul's.
        let mut replaced = good.clone();
        replaced.keys[1].key = seal_key(&ChatKey::generate(), &id, rn, &rk);
        assert!(refused(&replaced));
    }

    #[test]
    fn a_sealed_key_opens_only_in_its_place_for_its_recipient() {
        let (abin, rahul) = (Secrets::generate(), Secrets::generate());
        let (rn, other) = (number(222_222_222), number(333_333_333));
        let id = [9u8; 16];
        let key = ChatKey::generate();
        let sealed = seal_key(&key, &id, rn, &rahul.public_keys());
        assert_eq!(open_key(&sealed, &id, rn, &rahul).unwrap().0[..], key.0[..]);
        assert!(open_key(&sealed, &[8u8; 16], rn, &rahul).is_none());
        assert!(open_key(&sealed, &id, other, &rahul).is_none());
        assert!(open_key(&sealed, &id, rn, &abin).is_none());
    }

    #[test]
    fn the_seal_layout_is_the_agreed_one() {
        use hkdf::Hkdf;
        use sha2::Sha256;
        let rahul = Secrets::generate();
        let rn = number(222_222_222);
        let id = [9u8; 16];
        let key = [0x5au8; 32];

        let eph = StaticSecret::random_from_rng(OsRng);
        let eph_pub = PublicKey::from(&eph).to_bytes();
        let recipient = rahul.public_keys().exchange.0;
        let shared = eph.diffie_hellman(&PublicKey::from(recipient));
        let salt = [eph_pub.as_slice(), recipient.as_slice()].concat();
        let mut wrap = [0u8; 32];
        Hkdf::<Sha256>::new(Some(&salt), shared.as_bytes()).expand(b"corvo-chat seal v1", &mut wrap).unwrap();
        let aad = [b"corvo-chat seal v1".as_slice(), &id, rn.digits().as_bytes()].concat();
        let blob = Blob([eph_pub.to_vec(), crate::crypto::seal(&wrap, &aad, &key)].concat());

        assert_eq!(blob.0.len(), 104);
        assert_eq!(open_key(&blob, &id, rn, &rahul).unwrap().0[..], key[..]);
    }
}
