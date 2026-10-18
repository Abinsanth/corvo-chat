//! The password rules: long enough and not a well-known password.

use std::collections::HashSet;
use std::sync::OnceLock;

use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroizing;

use crate::Error;

const MIN_CHARS: usize = 10;

/// The bundled list of common passwords (see the header of the file).
static COMMON: &str = include_str!("passwords.txt");

fn common() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| COMMON.lines().filter(|l| !l.is_empty() && !l.starts_with('#')).collect())
}

/// A password in the one form used for OPAQUE and the key file, so the same
/// password typed on another keyboard still matches.
pub(crate) fn normalize(p: &str) -> Zeroizing<String> {
    Zeroizing::new(p.nfc().collect())
}

/// The sign-up rule: at least 10 characters, and not a well-known password.
pub fn check_password(p: &str) -> Result<(), Error> {
    let p = normalize(p);
    if p.chars().count() < MIN_CHARS {
        return Err(Error::PasswordTooShort);
    }
    // A copy of the password too, so it's wiped as well.
    let lower = Zeroizing::new(p.to_lowercase());
    if common().contains(lower.as_str()) {
        return Err(Error::PasswordTooCommon);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    #[test]
    fn password_rules() {
        assert!(matches!(check_password("short"), Err(Error::PasswordTooShort)));
        assert!(matches!(check_password("Password123"), Err(Error::PasswordTooCommon)));
        assert!(check_password("violet tram 44 north").is_ok());
        assert_eq!(normalize("e\u{301}").as_str(), "\u{e9}");
    }
}
