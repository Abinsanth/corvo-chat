//! The command line: the flags, and `corvo-chat --update` and `--uninstall`,
//! which run without the screen.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use clap::Parser;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use zeroize::Zeroizing;

use crate::files::KEY_FILE;
use crate::selfupdate::{self, Stage, Urls};
use crate::ui::runner::Server;
use crate::uninstall::{self, Install, PathEdit, SignOutError};

/// End-to-end encrypted chat in your terminal.
#[derive(Parser)]
#[command(version)]
pub struct Args {
    /// Install the newest release, if there is one
    #[arg(long)]
    pub update: bool,
    /// Remove corvo-chat from this computer (hidden: the installer's way out)
    #[arg(long, hide = true, conflicts_with = "update")]
    pub uninstall: bool,
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

/// Said when this computer isn't signed out on the server.
const LEFT_TO_EXPIRE: &str = "This computer leaves your Devices list after 30 days unused.";

/// `corvo-chat --uninstall`: signs this computer out if the password is given,
/// then removes the data folder, the PATH line and the binary. Nothing is
/// touched until the answer is yes. `Err` ends the program with a failure;
/// it's empty when the failures were already listed.
pub async fn uninstall(
    dir: &Path,
    exe: &Path,
    server: &Server,
    input: impl BufRead,
    out: impl Write,
) -> Result<(), String> {
    uninstall_in(&Install::here(dir, exe), server, input, out, &mut |input| hidden_line(input)).await
}

/// [`uninstall`] on `install`, reading the password with `ask`.
async fn uninstall_in<R: BufRead>(
    install: &Install,
    server: &Server,
    mut input: R,
    mut out: impl Write,
    ask: &mut impl FnMut(&mut R) -> io::Result<String>,
) -> Result<(), String> {
    let out = &mut out;
    let io_err = |e: io::Error| e.to_string();
    if install.dir.join(KEY_FILE).exists() {
        loop {
            say(out, "Password, to sign this computer out (Enter to skip): ")?;
            let password = Zeroizing::new(ask(&mut input).map_err(io_err)?);
            if password.is_empty() {
                say(out, &format!("Skipped. {LEFT_TO_EXPIRE}\n"))?;
                break;
            }
            match uninstall::sign_out_here(&install.dir, &password, server).await {
                Ok(()) => {
                    say(out, "Signed out.\n")?;
                    break;
                }
                Err(SignOutError::WrongPassword) => say(out, "Wrong password\n")?,
                Err(SignOutError::Failed(why)) => {
                    say(out, &format!("Couldn't sign out ({why}). {LEFT_TO_EXPIRE}\n"))?;
                    break;
                }
            }
        }
    }

    let folder = install.exe.parent().unwrap_or(Path::new("."));
    let from_source = selfupdate::is_built_from_source(&install.exe);
    let edits: Vec<_> = install.profiles.iter().map(|p| (p, uninstall::edit_profile(p, folder))).collect();
    let mut places = vec![install.dir.display().to_string()];
    if !from_source {
        places.push(install.exe.display().to_string());
    }
    let path_in: Vec<String> = edits
        .iter()
        .filter(|(_, edit)| matches!(edit, Ok(PathEdit::Removed(_))))
        .map(|(p, _)| p.display().to_string())
        .collect();
    if !path_in.is_empty() {
        places.push(format!("the PATH line in {}", path_in.join(", ")));
    } else if uninstall::user_path_added(folder) {
        places.push("the PATH entry".into());
    }
    let list = match places.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{}, and {last}", rest.join(", ")),
        _ => places.join(""),
    };
    say(out, &format!("This removes corvo-chat from this computer: {list}. Your account stays. Go on? y/n: "))?;
    let mut answer = String::new();
    input.read_line(&mut answer).map_err(io_err)?;
    if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
        return say(out, "Nothing changed.\n");
    }

    let mut failed = Vec::new();
    let mut notes = Vec::new();
    if let Err(why) = uninstall::remove_data(&install.dir, install.home.as_deref()) {
        failed.push(format!("{}: {why}", install.dir.display()));
    }
    for (profile, edit) in &edits {
        match edit {
            Ok(PathEdit::Changed) => {
                say(out, &format!("Check {}: the PATH line there was changed, so it was left.\n", profile.display()))?
            }
            Ok(PathEdit::Removed(text)) => {
                if let Err(e) = std::fs::write(profile, text) {
                    failed.push(format!("{}: {e}", profile.display()));
                }
            }
            Ok(PathEdit::NotThere) => {}
            Err(why) => failed.push(format!("{}: {why}", profile.display())),
        }
    }
    if let Err(why) = uninstall::remove_user_path_entry(folder) {
        failed.push(format!("the PATH entry: {why}"));
    }
    if from_source {
        say(out, &format!("Left {}: it's a build from source.\n", install.exe.display()))?;
    } else {
        uninstall::remove_binary(&install.exe, &mut failed, &mut notes);
    }

    if failed.is_empty() {
        say(
            out,
            "corvo-chat is removed from this computer. Your account still exists: install again and sign in to get your chats back.\n",
        )?;
        for note in notes {
            say(out, &format!("{note}\n"))?;
        }
        return Ok(());
    }
    say(out, "Removed what it could. Not removed:\n")?;
    for line in failed.iter().chain(&notes) {
        say(out, &format!("  {line}\n"))?;
    }
    Err(String::new())
}

fn say(out: &mut impl Write, text: &str) -> Result<(), String> {
    write!(out, "{text}").and_then(|()| out.flush()).map_err(|e| e.to_string())
}

/// One line, without its end. Empty at the end of the input.
fn plain_line(input: &mut impl BufRead) -> io::Result<String> {
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// A line typed at the terminal without showing it; from `input` when that
/// isn't a terminal (a pipe).
fn hidden_line(input: &mut impl BufRead) -> io::Result<String> {
    if !io::stdin().is_terminal() {
        return plain_line(input);
    }
    enable_raw_mode()?;
    let typed = typed_secret();
    let _ = disable_raw_mode();
    println!();
    typed
}

fn typed_secret() -> io::Result<String> {
    let mut typed = String::new();
    loop {
        let Event::Key(key) = event::read()? else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => return Ok(typed),
            KeyCode::Backspace => {
                typed.pop();
            }
            KeyCode::Char('c') if ctrl => return Err(io::Error::new(io::ErrorKind::Interrupted, "Interrupted")),
            KeyCode::Char('d') if ctrl && typed.is_empty() => return Ok(typed),
            KeyCode::Char(c) if !ctrl => typed.push(c),
            _ => {}
        }
    }
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

    /// `--uninstall`: against temp folders, fake exes and fake profiles only.
    mod uninstall {
        use corvo_chat_core::native::NativeTransport;
        use corvo_chat_core::{Error, LocalKey, hello, resume};

        use super::*;
        use crate::ui::runner::Server;
        use crate::ui::runner::testing::{App, PASSWORD, TestServer};
        use crate::uninstall::Install;

        const MARK: &str = "# added by the corvo-chat installer";

        /// Data folder, fake exe and fake profile in temp folders.
        struct Setup {
            _tmp: tempfile::TempDir,
            install: Install,
        }

        impl Setup {
            fn new(with_key: bool) -> Setup {
                let tmp = tempfile::tempdir().unwrap();
                let (dir, bin, home) = (tmp.path().join("data"), tmp.path().join("bin"), tmp.path().join("home"));
                for d in [&dir, &bin, &home] {
                    fs::create_dir_all(d).unwrap();
                }
                fs::write(dir.join("cache.db"), "chats").unwrap();
                if with_key {
                    fs::write(dir.join("key.json"), "{}").unwrap();
                }
                let exe = bin.join("corvo-chat");
                fs::write(&exe, "binary").unwrap();
                let profile = home.join(".zshrc");
                let line = format!("export PATH='{}':\"$PATH\"", bin.display());
                fs::write(&profile, format!("export A=1\n\n{MARK}\n{line}\nexport B=2\n")).unwrap();
                Setup { install: Install { dir, exe, home: Some(home), profiles: vec![profile] }, _tmp: tmp }
            }

            fn profile(&self) -> String {
                fs::read_to_string(&self.install.profiles[0]).unwrap()
            }

            async fn run(&self, answers: &str) -> (Result<(), String>, String) {
                self.run_at(&Server::default(), answers).await
            }

            async fn run_at(&self, server: &Server, answers: &str) -> (Result<(), String>, String) {
                let mut out = Vec::new();
                let done =
                    uninstall_in(&self.install, server, answers.as_bytes(), &mut out, &mut |i| plain_line(i)).await;
                (done, output(out))
            }
        }

        #[tokio::test]
        async fn uninstall_signs_out_then_removes_everything() {
            let server = TestServer::start().await;
            let setup = Setup::new(false);
            let dir = &setup.install.dir;
            fs::remove_dir_all(dir).unwrap();
            let app = App::active(dir, &server, "ana").await;
            drop(app);
            let key = LocalKey::from_json(&fs::read_to_string(dir.join("key.json")).unwrap()).unwrap();
            let (_, token) = key.unlock(PASSWORD).unwrap();

            let answers = format!("not the password\n{PASSWORD}\ny\n");
            let (done, said) = setup.run_at(&server.server, &answers).await;
            done.unwrap();
            assert!(said.contains("Password, to sign this computer out (Enter to skip): "), "{said}");
            assert!(said.contains("Wrong password\n"), "{said}");
            assert!(said.contains(&format!("This removes corvo-chat from this computer: {}", dir.display())), "{said}");
            assert!(
                said.contains(&format!(
                    "and the PATH line in {}. Your account stays. Go on? y/n: ",
                    setup.install.profiles[0].display()
                )),
                "{said}"
            );
            assert!(said.ends_with(
                "corvo-chat is removed from this computer. Your account still exists: install again and sign in to get your chats back.\n"
            ), "{said}");
            assert!(!dir.exists());
            assert!(!setup.install.exe.exists());
            assert_eq!(setup.profile(), "export A=1\nexport B=2\n");

            // The server no longer knows this device's session.
            let mut t = NativeTransport::connect(&server.server.url, server.server.roots.clone()).await.unwrap();
            hello(&mut t, "0.0.0").await.unwrap();
            let resumed = resume(&mut t, &token, &key.device_id).await;
            assert!(matches!(resumed, Err(Error::SignedOut(_))), "{resumed:?}");
        }

        #[tokio::test]
        async fn uninstall_without_a_key_file_skips_the_password() {
            let setup = Setup::new(false);
            let (done, said) = setup.run("y\n").await;
            done.unwrap();
            assert!(!said.contains("Password"), "{said}");
            assert!(!setup.install.dir.exists());
            assert!(!setup.install.exe.exists());
        }

        #[tokio::test]
        async fn uninstall_says_no_and_changes_nothing() {
            let setup = Setup::new(true);
            let before = setup.profile();
            let (done, said) = setup.run("\nn\n").await;
            done.unwrap();
            assert!(said.contains("Skipped. This computer leaves your Devices list after 30 days unused."), "{said}");
            assert!(said.contains("Nothing changed."), "{said}");
            assert!(setup.install.dir.join("key.json").exists());
            assert!(setup.install.exe.exists());
            assert_eq!(setup.profile(), before);
        }

        #[tokio::test]
        async fn uninstall_says_when_it_cannot_sign_out_and_goes_on() {
            let setup = Setup::new(true);
            let (done, said) = setup.run("whatever\ny\n").await;
            done.unwrap();
            assert!(said.contains("Couldn't sign out ("), "{said}");
            assert!(said.contains("). This computer leaves your Devices list after 30 days unused."), "{said}");
            assert!(!setup.install.dir.exists());
        }

        #[tokio::test]
        async fn uninstall_leaves_a_path_line_that_was_edited() {
            let setup = Setup::new(false);
            let edited = format!("a\n{MARK}\nexport PATH='/else':\"$PATH\"\n");
            fs::write(&setup.install.profiles[0], &edited).unwrap();
            let (done, said) = setup.run("y\n").await;
            done.unwrap();
            let profile = setup.install.profiles[0].display();
            assert!(
                said.contains(&format!("Check {profile}: the PATH line there was changed, so it was left.")),
                "{said}"
            );
            assert_eq!(setup.profile(), edited);
            assert!(!setup.install.exe.exists());
        }

        #[tokio::test]
        async fn uninstall_leaves_a_build_from_source() {
            let setup = Setup::new(false);
            let exe = setup.install.dir.parent().unwrap().join("target/debug/corvo-chat");
            fs::create_dir_all(exe.parent().unwrap()).unwrap();
            fs::write(&exe, "binary").unwrap();
            let install = Install { exe: exe.clone(), ..setup.install.clone() };
            let mut out = Vec::new();
            let done = uninstall_in(&install, &Server::default(), &b"y\n"[..], &mut out, &mut |i| plain_line(i)).await;
            done.unwrap();
            assert!(output(out).contains(&format!("Left {}: it's a build from source.", exe.display())));
            assert!(exe.exists());
            assert!(!install.dir.exists());
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn what_it_cant_remove_is_listed() {
            use std::os::unix::fs::PermissionsExt;
            let setup = Setup::new(false);
            let bin = setup.install.exe.parent().unwrap().to_path_buf();
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o555)).unwrap();
            let (done, said) = setup.run("y\n").await;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(done.is_err(), "{said}");
            assert!(said.contains("Removed what it could. Not removed:"), "{said}");
            assert!(said.contains(&setup.install.exe.display().to_string()), "{said}");
            assert!(!said.contains("is removed from this computer"), "{said}");
            assert!(!setup.install.dir.exists(), "the data went anyway");
            assert!(setup.install.exe.exists());
        }
    }
}
