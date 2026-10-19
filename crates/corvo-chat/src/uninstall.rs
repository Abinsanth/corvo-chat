//! The pieces of `corvo-chat --uninstall` (the flow is in `cli`): signing this
//! computer out, and taking the files, the PATH line and the binary away.
//! Everything works on the paths it's given, so tests use temp folders.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use corvo_chat_core::native::NativeTransport;
use corvo_chat_core::{Client, Error, LocalKey, MemoryCache, SignedIn, Strength, hello, resume};
use corvo_chat_proto::B64;
use corvo_chat_proto::frames::{Device, DeviceKind};

use crate::files::{KEY_FILE, device_name, home_dir};
use crate::selfupdate;
use crate::ui::runner::Server;
use crate::ui::state::VERSION;

/// The line before the PATH line, as install.sh writes it.
const MARKER: &str = "# added by the corvo-chat installer";

/// How long signing out gets, connecting included.
const SIGN_OUT_WAIT: Duration = Duration::from_secs(15);

/// What `--uninstall` works on.
#[derive(Debug, Clone)]
pub struct Install {
    /// The data folder.
    pub dir: PathBuf,
    /// The binary.
    pub exe: PathBuf,
    /// The home folder, which is never removed.
    pub home: Option<PathBuf>,
    /// The shell profiles that may hold the installer's PATH line.
    pub profiles: Vec<PathBuf>,
}

impl Install {
    /// This computer's: the profiles are those in the real home folder.
    pub fn here(dir: &Path, exe: &Path) -> Install {
        let home = home_dir();
        let profiles = match &home {
            Some(home) if !cfg!(windows) => profiles(home),
            _ => Vec::new(),
        };
        Install { dir: dir.to_path_buf(), exe: exe.to_path_buf(), home, profiles }
    }
}

#[derive(Debug)]
pub enum SignOutError {
    WrongPassword,
    Failed(String),
}

/// Signs this device out on the server the key file in `dir` names, using the
/// session saved there, after checking `password` opens it. Only the server's
/// certificate authorities come from `server`.
pub async fn sign_out_here(dir: &Path, password: &str, server: &Server) -> Result<(), SignOutError> {
    let failed = |e: &dyn std::fmt::Display| SignOutError::Failed(e.to_string());
    let json = fs::read_to_string(dir.join(KEY_FILE)).map_err(|e| failed(&e))?;
    let key = LocalKey::from_json(&json).map_err(|e| failed(&e))?;
    let (secrets, token) = key.unlock(password).map_err(|e| match e {
        Error::WrongPassword => SignOutError::WrongPassword,
        other => failed(&other),
    })?;
    let signing_out = async {
        let mut t = NativeTransport::connect(&key.server, server.roots.clone()).await?;
        hello(&mut t, VERSION).await?;
        let account = resume(&mut t, &token, &key.device_id).await?;
        let device = Device { id: B64(key.device_id), kind: DeviceKind::Cli, name: device_name() };
        let signed_in = SignedIn { token, account, secrets };
        Client::new(t, signed_in, device, Strength::Normal, MemoryCache::new()).sign_out().await
    };
    match tokio::time::timeout(SIGN_OUT_WAIT, signing_out).await {
        Ok(Ok(())) => Ok(()),
        // Already signed out is as good.
        Ok(Err(Error::SignedOut(_))) => Ok(()),
        Ok(Err(e)) => Err(failed(&e)),
        Err(_) => Err(SignOutError::Failed("the server didn't answer in time".into())),
    }
}

/// Removes the data folder and everything in it. Not there is fine. Refuses
/// an empty or relative path, the root, and `home` itself.
pub fn remove_data(dir: &Path, home: Option<&Path>) -> Result<(), String> {
    if dir.as_os_str().is_empty() || !dir.is_absolute() || dir.parent().is_none() || Some(dir) == home {
        return Err(format!("won't remove {}: it isn't a data folder", dir.display()));
    }
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// What taking the installer's PATH line out of a profile did.
#[derive(Debug, PartialEq, Eq)]
pub enum PathEdit {
    /// The profile without the marker, the line under it and one empty line before.
    Removed(String),
    /// The installer's marker isn't there.
    NotThere,
    /// The marker is there but the line under it isn't what the installer wrote.
    Changed,
}

/// Removes the marker line and the PATH line under it, as install.sh wrote it
/// for `folder`: `export PATH='<folder>':"$PATH"`, or the older
/// `export PATH="<folder>:$PATH"` (`folder` can also be the whole line). The
/// empty line install.sh writes before the marker goes with it.
pub fn without_path_entry(profile: &str, folder: &str) -> PathEdit {
    let lines: Vec<&str> = profile.split_inclusive('\n').collect();
    let text = |line: &str| line.trim_end_matches(['\n', '\r']).to_string();
    let ours = [
        format!("export PATH='{}':\"$PATH\"", folder.replace('\'', "'\\''")),
        format!("export PATH=\"{folder}:$PATH\""),
        folder.to_string(),
    ];
    let mut keep = vec![true; lines.len()];
    let mut marked = false;
    for (i, line) in lines.iter().enumerate() {
        if text(line) != MARKER {
            continue;
        }
        marked = true;
        if lines.get(i + 1).is_some_and(|next| ours.contains(&text(next))) {
            keep[i] = false;
            keep[i + 1] = false;
            if i > 0 && text(lines[i - 1]).is_empty() {
                keep[i - 1] = false;
            }
        }
    }
    if !marked {
        PathEdit::NotThere
    } else if keep.iter().all(|k| *k) {
        PathEdit::Changed
    } else {
        PathEdit::Removed(lines.iter().zip(&keep).filter(|(_, k)| **k).map(|(l, _)| *l).collect())
    }
}

/// What taking the line out of `profile` would do, when it can be read.
pub fn edit_profile(profile: &Path, folder: &Path) -> Result<PathEdit, String> {
    match fs::read_to_string(profile) {
        Ok(text) => Ok(without_path_entry(&text, &folder.to_string_lossy())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(PathEdit::NotThere),
        Err(e) => Err(e.to_string()),
    }
}

/// The profiles install.sh may have written: `.zshrc` under `$ZDOTDIR` (and
/// in home), `.bashrc`, and the macOS login files.
pub fn profiles(home: &Path) -> Vec<PathBuf> {
    profiles_in(home, std::env::var("ZDOTDIR").ok().as_deref())
}

fn profiles_in(home: &Path, zdotdir: Option<&str>) -> Vec<PathBuf> {
    let zdotdir = zdotdir.map(PathBuf::from).filter(|p| p.is_absolute());
    let mut list: Vec<PathBuf> = zdotdir.into_iter().map(|z| z.join(".zshrc")).collect();
    list.push(home.join(".zshrc"));
    list.extend([".bashrc", ".bash_profile", ".bash_login", ".profile"].map(|f| home.join(f)));
    list.dedup();
    list
}

/// Takes the binary away. It's moved aside first, since Windows can rename a
/// running exe but not delete it. Failures go to `failed`; a moved-aside exe
/// that's still running goes to `notes`.
pub fn remove_binary(exe: &Path, failed: &mut Vec<String>, notes: &mut Vec<String>) {
    let aside = match selfupdate::set_aside(exe) {
        Ok(aside) => aside,
        Err(e) if e.kind() == ErrorKind::NotFound => return,
        Err(e) => {
            failed.push(format!("{}: {e}", exe.display()));
            return;
        }
    };
    let Err(e) = fs::remove_file(&aside) else {
        #[cfg(windows)]
        remove_install_folder(exe);
        return;
    };
    if cfg!(windows) {
        let folder = exe.parent().unwrap_or(Path::new("."));
        notes.push(format!("{} is still in use; delete the folder {} by hand.", aside.display(), folder.display()));
    } else {
        failed.push(format!("{}: {e}", aside.display()));
    }
}

/// Removes the default install folder, `%LOCALAPPDATA%\Programs\corvo-chat`,
/// when nothing is left in it.
#[cfg(windows)]
fn remove_install_folder(exe: &Path) {
    let default = std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("Programs").join("corvo-chat"));
    if exe.parent().is_some() && exe.parent() == default.as_deref() {
        let _ = fs::remove_dir(exe.parent().unwrap_or(Path::new(".")));
    }
}

/// Whether install.ps1 added `folder` to the user's PATH (it says so in
/// `HKCU\Software\corvo-chat`, value `PathAdded`).
#[cfg(windows)]
pub fn user_path_added(folder: &Path) -> bool {
    let out = std::process::Command::new("reg").args(["query", REG_KEY, "/v", REG_VALUE]).output();
    let Ok(out) = out else { return false };
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().filter_map(|l| l.trim().strip_prefix(REG_VALUE)).filter_map(|l| l.split_once("REG_SZ")).any(
        |(_, value)| {
            value
                .trim()
                .trim_end_matches(['\\', '/'])
                .eq_ignore_ascii_case(folder.to_string_lossy().trim_end_matches(['\\', '/']))
        },
    )
}

#[cfg(not(windows))]
pub fn user_path_added(_folder: &Path) -> bool {
    false
}

#[cfg(windows)]
const REG_KEY: &str = r"HKCU\Software\corvo-chat";
#[cfg(windows)]
const REG_VALUE: &str = "PathAdded";

/// Takes `folder` off the user's PATH when install.ps1 added it, and forgets that it did.
#[cfg(windows)]
pub fn remove_user_path_entry(folder: &Path) -> Result<(), String> {
    remove_user_path(folder).map(|_| ())
}

#[cfg(not(windows))]
pub fn remove_user_path_entry(_folder: &Path) -> Result<(), String> {
    Ok(())
}

/// `true` if the PATH entry was there and is gone.
#[cfg(windows)]
fn remove_user_path(dir: &Path) -> Result<bool, String> {
    if !user_path_added(dir) {
        return Ok(false);
    }
    let dir = dir.to_string_lossy().replace('\'', "''");
    let script = format!(
        "$d = '{dir}'.TrimEnd('\\'); \
         $p = [Environment]::GetEnvironmentVariable('Path', 'User'); \
         $n = ($p -split ';' | Where-Object {{ $_ -and $_.TrimEnd('\\') -ne $d }}) -join ';'; \
         [Environment]::SetEnvironmentVariable('Path', $n, 'User')"
    );
    let run = |program: &str, args: &[&str]| -> Result<(), String> {
        let out = std::process::Command::new(program).args(args).output().map_err(|e| e.to_string())?;
        if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
    };
    run("powershell", &["-NoProfile", "-Command", &script])?;
    run("reg", &["delete", REG_KEY, "/v", REG_VALUE, "/f"])?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;

    const MARK: &str = "# added by the corvo-chat installer";

    #[test]
    fn without_path_entry_removes_only_the_installers_lines() {
        let profile = format!("a\n\n{MARK}\nexport PATH='/x/bin':\"$PATH\"\nb\n");
        assert_eq!(without_path_entry(&profile, "/x/bin"), PathEdit::Removed("a\nb\n".into()));
        // The old form, and a folder with a quote in its name.
        let old = format!("a\n\n{MARK}\nexport PATH=\"/x/bin:$PATH\"\n");
        assert_eq!(without_path_entry(&old, "/x/bin"), PathEdit::Removed("a\n".into()));
        let quoted = format!("{MARK}\nexport PATH='/it'\\''s/bin':\"$PATH\"\nb\n");
        assert_eq!(without_path_entry(&quoted, "/it's/bin"), PathEdit::Removed("b\n".into()));
        // A line before the marker that isn't empty stays.
        let kept = format!("a\n{MARK}\nexport PATH='/x/bin':\"$PATH\"\n");
        assert_eq!(without_path_entry(&kept, "/x/bin"), PathEdit::Removed("a\n".into()));
    }

    #[test]
    fn an_edited_line_is_left_alone() {
        let profile = format!("a\n\n{MARK}\nexport PATH='/y/bin':\"$PATH\"\nb\n");
        assert_eq!(without_path_entry(&profile, "/x/bin"), PathEdit::Changed);
        assert_eq!(without_path_entry(&format!("a\n{MARK}"), "/x/bin"), PathEdit::Changed);
    }

    #[test]
    fn a_profile_without_the_marker_is_not_touched() {
        assert_eq!(without_path_entry("export PATH='/x/bin':\"$PATH\"\n", "/x/bin"), PathEdit::NotThere);
        assert_eq!(without_path_entry("", "/x/bin"), PathEdit::NotThere);
    }

    #[test]
    fn the_profiles_are_the_ones_the_installer_may_write() {
        let home = Path::new("/h");
        let names = |zdotdir| -> Vec<PathBuf> { profiles_in(home, zdotdir) };
        assert_eq!(
            names(None),
            ["/h/.zshrc", "/h/.bashrc", "/h/.bash_profile", "/h/.bash_login", "/h/.profile"].map(PathBuf::from)
        );
        assert_eq!(names(Some("/z"))[..2], ["/z/.zshrc", "/h/.zshrc"].map(PathBuf::from));
        assert_eq!(names(Some("rel")), names(None), "a relative ZDOTDIR is ignored");
    }

    #[test]
    fn remove_data_removes_the_folder_and_what_is_in_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/key.json"), "x").unwrap();
        remove_data(&dir, Some(Path::new("/home/ana"))).unwrap();
        assert!(!dir.exists());
        remove_data(&dir, None).unwrap();
    }

    #[test]
    fn remove_data_refuses_the_root_the_home_and_odd_paths() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("keep"), "x").unwrap();
        let home = tmp.path();
        for bad in [Path::new(""), Path::new("/"), home, Path::new("relative/data")] {
            assert!(remove_data(bad, Some(home)).is_err(), "{bad:?}");
        }
        assert!(tmp.path().join("keep").exists());
    }
}
