//! The command line: the flags, and `corvo-chat --update`, which runs without
//! the data folder or the screen.

use std::io::{BufRead, Write};
use std::path::Path;

use clap::Parser;

use crate::selfupdate::{self, Stage, Urls};

/// End-to-end encrypted chat in your terminal.
#[derive(Parser)]
#[command(version)]
pub struct Args {
    /// Install the newest release, if there is one
    #[arg(long)]
    pub update: bool,
}

/// Said by the screen when the server needs a newer app, and by sign in and sign up.
pub const TOO_OLD: &str = "This version is too old. Quit and run: corvo-chat --update";

/// Said on Windows when the swap fails because the exe is in use.
const CLOSE_FIRST: &str = "Close corvo-chat first, then run it again.";

/// `corvo-chat --update`: says what's newest, asks, and installs over `exe`.
/// What it says goes to `out`; the answer is read from `input`. `Err` is a
/// message for the user, which ends the program with a failure.
pub async fn update(urls: &Urls, exe: &Path, input: impl BufRead, out: impl Write) -> Result<(), String> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let asset = selfupdate::asset_name(os, arch).ok_or_else(|| selfupdate::no_build(os, arch))?;
    update_asset(urls, &asset, true, exe, input, out).await
}

/// [`update`] for `asset`, the release file for this system. With `verify`, the
/// new binary is run before it's put in place.
async fn update_asset(
    urls: &Urls,
    asset: &str,
    verify: bool,
    exe: &Path,
    mut input: impl BufRead,
    mut out: impl Write,
) -> Result<(), String> {
    let mut say = |text: &str| write!(out, "{text}").and_then(|()| out.flush()).map_err(|e| e.to_string());
    if selfupdate::is_built_from_source(exe) {
        return say(&format!("{}\n", selfupdate::FROM_SOURCE));
    }
    let current = selfupdate::current();
    let Some(release) = selfupdate::check_with(urls, &current, asset).await? else {
        return say(&format!("corvo-chat {current} is the newest. Nothing to do.\n"));
    };
    let version = &release.version;
    say(&format!("corvo-chat {version} is out (you have {current}). Update? y/n: "))?;
    let mut answer = String::new();
    input.read_line(&mut answer).map_err(|e| e.to_string())?;
    if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
        return say("Nothing changed.\n");
    }
    let mut fetched = None;
    let mut stage = Some(Stage::Fetch);
    while let Some(now) = stage {
        match now {
            Stage::Fetch => say("Downloading…  ")?,
            Stage::Check => say("Checking…  ")?,
            Stage::Swap => say("Installing…\n")?,
        }
        match (now, fetched.take()) {
            (Stage::Fetch, _) => fetched = Some(selfupdate::fetch(urls, &release, asset, exe).await?),
            (Stage::Check, Some(got)) => {
                selfupdate::check(&got, version, verify).await?;
                fetched = Some(got);
            }
            (Stage::Swap, Some(got)) => selfupdate::swap(got, exe).map_err(in_use_hint)?,
            _ => unreachable!("each stage follows the one before"),
        }
        stage = now.next();
    }
    say(&format!("Updated to {version}.\n"))
}

/// A swap's failure, telling Windows users to close the app when that's why.
fn in_use_hint(why: String) -> String {
    if cfg!(windows) && why.contains("os error 32") { format!("{why} {CLOSE_FIRST}") } else { why }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::selfupdate::testing::{VERIFY, file_url, publish};

    fn output(out: Vec<u8>) -> String {
        String::from_utf8(out).unwrap()
    }

    #[tokio::test]
    async fn update_says_nothing_to_do_when_newest() {
        let site = tempfile::tempdir().unwrap();
        fs::write(site.path().join("releases.json"), "[]").unwrap();
        let urls = Urls { api: file_url(&site.path().join("releases.json")), download: file_url(site.path()) };
        let mut out = Vec::new();
        update_asset(&urls, "corvo-chat-test.tar.gz", VERIFY, Path::new("/bin/corvo-chat"), &b""[..], &mut out)
            .await
            .unwrap();
        assert!(output(out).contains("is the newest. Nothing to do."));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn update_installs_after_yes() {
        let (site, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (urls, asset) = publish(site.path(), true);
        let exe = bin.path().join("corvo-chat");
        fs::write(&exe, "old").unwrap();
        let mut out = Vec::new();
        update_asset(&urls, &asset, VERIFY, &exe, &b"y\n"[..], &mut out).await.unwrap();
        let said = output(out);
        assert!(said.contains("corvo-chat 9.0.0 is out (you have 0.1.0). Update? y/n: "), "{said}");
        assert!(said.contains("Downloading…  Checking…  Installing…\nUpdated to 9.0.0."), "{said}");
        assert_eq!(
            fs::read_to_string(&exe).unwrap(),
            fs::read_to_string(site.path().join("staging/corvo-chat")).unwrap()
        );
    }

    #[tokio::test]
    async fn update_changes_nothing_after_no() {
        let (site, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (urls, asset) = publish(site.path(), true);
        let exe = bin.path().join("corvo-chat");
        fs::write(&exe, "old").unwrap();
        for answer in ["\n", "n\n", ""] {
            let mut out = Vec::new();
            update_asset(&urls, &asset, VERIFY, &exe, answer.as_bytes(), &mut out).await.unwrap();
            assert!(output(out).contains("Nothing changed."));
            assert_eq!(fs::read_to_string(&exe).unwrap(), "old");
        }
    }

    #[tokio::test]
    async fn update_refuses_from_a_checkout() {
        let site = tempfile::tempdir().unwrap();
        let (urls, asset) = publish(site.path(), true);
        let mut out = Vec::new();
        let exe = Path::new("/work/corvo-chat-client/target/release/corvo-chat");
        update_asset(&urls, &asset, VERIFY, exe, &b"y\n"[..], &mut out).await.unwrap();
        assert_eq!(output(out).trim(), selfupdate::FROM_SOURCE);
    }
}
