//! The recovery code: 15 random bytes shown as 24 Crockford base32 characters.

use rand_core::{OsRng, RngCore};
use zeroize::Zeroizing;

use crate::Error;

/// Crockford's base32: no I, L, O or U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CHARS: usize = 24;

/// A recovery code. The 24 uppercase characters, without dashes, are also the
/// OPAQUE secret of the recovery record.
pub struct RecoveryCode(Zeroizing<String>);

impl std::fmt::Debug for RecoveryCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecoveryCode(…)")
    }
}

impl RecoveryCode {
    /// A fresh code: 15 random bytes (120 bits).
    pub fn generate() -> RecoveryCode {
        let mut bytes = Zeroizing::new([0u8; 15]);
        OsRng.fill_bytes(bytes.as_mut());
        let mut text = Zeroizing::new(String::with_capacity(CHARS));
        // Five bytes make 40 bits, which is eight characters.
        for chunk in bytes.chunks(5) {
            let bits = u64::from_be_bytes([0, 0, 0, chunk[0], chunk[1], chunk[2], chunk[3], chunk[4]]);
            for i in (0..8).rev() {
                text.push(ALPHABET[((bits >> (i * 5)) & 31) as usize] as char);
            }
        }
        RecoveryCode(text)
    }

    /// Reads a typed code. Case, spaces and dashes don't matter, and `I`/`L`
    /// count as `1` and `O` as `0`, as Crockford's base32 says.
    pub fn parse(typed: &str) -> Result<RecoveryCode, Error> {
        let mut text = Zeroizing::new(String::with_capacity(CHARS));
        for c in typed.chars() {
            if c == ' ' || c == '-' {
                continue;
            }
            let c = match c.to_ascii_uppercase() {
                'I' | 'L' => '1',
                'O' => '0',
                c => c,
            };
            if !c.is_ascii() || !ALPHABET.contains(&(c as u8)) || text.len() == CHARS {
                return Err(Error::BadRecoveryCode);
            }
            text.push(c);
        }
        if text.len() != CHARS {
            return Err(Error::BadRecoveryCode);
        }
        Ok(RecoveryCode(text))
    }

    /// The code in six groups of four, joined by dashes.
    pub fn display(&self) -> String {
        let groups: Vec<&str> = (0..CHARS).step_by(4).map(|i| &self.0[i..i + 4]).collect();
        groups.join("-")
    }

    /// The last group, which the sign-up flow asks the person to type back.
    pub fn last_group(&self) -> &str {
        &self.0[CHARS - 4..]
    }

    /// The OPAQUE secret.
    pub(crate) fn secret(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    #[test]
    fn recovery_code_round_trips_and_forgives_typing() {
        let code = RecoveryCode::generate();
        let shown = code.display();
        assert_eq!(shown.len(), 29);
        assert_eq!(shown.matches('-').count(), 5);
        assert_eq!(code.last_group(), &shown[25..]);
        let typed = shown.to_lowercase().replace('-', " ").replace('0', "o").replace('1', "l");
        assert_eq!(RecoveryCode::parse(&typed).unwrap().secret(), code.secret());
        assert!(matches!(RecoveryCode::parse("UUUU-UUUU-UUUU-UUUU-UUUU-UUUU"), Err(Error::BadRecoveryCode)));
        assert!(matches!(RecoveryCode::parse("ABCD"), Err(Error::BadRecoveryCode)));
    }
}
