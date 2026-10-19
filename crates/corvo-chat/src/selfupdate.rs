//! `corvo-chat --update`: finds a newer GitHub release and puts it in place of the running
//! binary. Talks to GitHub through the system `curl` and unpacks with the
//! system `tar`, as install.sh does (Windows 10+ ships both), so the app needs
//! no HTTP or TLS crates.

// Nothing calls this yet: `--update` and `--uninstall` (the next tasks) use it.
#![allow(dead_code)]

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const REPO: &str = "Abinsanth/corvo-chat-client";
const INSTALL_URL: &str = "https://raw.githubusercontent.com/Abinsanth/corvo-chat-client/release/install.sh";
const INSTALL_PS1_URL: &str = "https://raw.githubusercontent.com/Abinsanth/corvo-chat-client/release/install.ps1";
/// The binary inside a release archive.
#[cfg(windows)]
const BIN_NAME: &str = "corvo-chat.exe";
#[cfg(not(windows))]
const BIN_NAME: &str = "corvo-chat";
/// Names the temp folder an update works in, beside the binary it replaces.
const TMP_PREFIX: &str = ".corvo-chat-update-";
/// Where the running exe is moved aside (Windows can rename it but not replace it):
/// `corvo-chat.old-<n>.exe`.
const OLD_EXE_PREFIX: &str = "corvo-chat.old";
/// Held locked in an update's temp folder while it works, so another
/// update's sweep leaves the folder alone.
const LOCK_FILE: &str = ".lock";
/// How long the new binary gets to print its version before the swap.
const VERSION_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// Said instead of updating a binary built from a checkout.
pub const FROM_SOURCE: &str = "built from source; update with git pull && cargo build";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// The release's tag, as its downloads are found under (`v0.1.0`, or a tag without the `v`).
    pub tag: String,
    /// The downloadable files.
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Asset {
    pub name: String,
    /// Bytes.
    pub size: u64,
}

/// The fields used from `GET /repos/{REPO}/releases`.
#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

/// The version of this binary.
pub fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is a semver version")
}

/// Published releases with a version tag, newest first, pre-releases
/// included (see `newest_above` for who is offered them).
pub fn parse_releases(json: &str) -> Result<Vec<Release>, String> {
    let api: Vec<ApiRelease> = serde_json::from_str(json).map_err(|_| "unexpected reply from GitHub".to_string())?;
    let mut releases: Vec<Release> = api
        .into_iter()
        .filter(|r| !r.draft)
        .filter_map(|r| {
            let version = Version::parse(r.tag_name.strip_prefix('v').unwrap_or(&r.tag_name)).ok()?;
            Some(Release { version, tag: r.tag_name, assets: r.assets })
        })
        .collect();
    releases.sort_by(|a, b| b.version.cmp(&a.version));
    Ok(releases)
}

/// The newest of `releases` above `current` that has `asset`, this system's build,
/// and its checksum.
/// Pre-releases are offered only to someone already on one.
pub fn newest_above(current: &Version, releases: Vec<Release>, asset: &str) -> Option<Release> {
    let testing = !current.pre.is_empty();
    releases
        .into_iter()
        .filter(|r| r.version > *current && (testing || r.version.pre.is_empty()))
        .filter(|r| has_with_checksum(r, asset))
        .max_by(|a, b| a.version.cmp(&b.version))
}

/// Whether `release` has `asset` and its `.sha256`: without the checksum an
/// update couldn't be checked, so such a release isn't offered.
fn has_with_checksum(release: &Release, asset: &str) -> bool {
    let sums = format!("{asset}.sha256");
    let has = |name: &str| release.assets.iter().any(|a| a.name == name);
    has(asset) && has(&sums)
}

/// What `--update` says on a system no release is built for.
pub fn no_build(os: &str, arch: &str) -> String {
    format!("no release build for {os}/{arch}; build from source")
}

/// Whether `exe` is in a cargo build folder (`target/debug`, `target/release`,
/// or the same under `target/<triple>`), so it came from a checkout.
pub fn is_built_from_source(exe: &Path) -> bool {
    let dirs: Vec<&OsStr> = exe.parent().map(|p| p.iter().collect()).unwrap_or_default();
    let Some((last, before)) = dirs.split_last() else { return false };
    let profile = matches!(last.to_str(), Some("debug" | "release"));
    let target_at = |back: usize| before.len() >= back && before[before.len() - back] == "target";
    profile && (target_at(1) || target_at(2))
}

/// The release asset for `std::env::consts::{OS, ARCH}`; named as install.sh and install.ps1 name them.
pub fn asset_name(os: &str, arch: &str) -> Option<String> {
    match (os, arch) {
        ("macos", "x86_64" | "aarch64") => Some(format!("corvo-chat-{arch}-apple-darwin.tar.gz")),
        ("linux", "x86_64" | "aarch64") => Some(format!("corvo-chat-{arch}-unknown-linux-musl.tar.gz")),
        ("windows", "x86_64") => Some("corvo-chat-x86_64-pc-windows-msvc.zip".into()),
        _ => None,
    }
}

/// Where releases are listed and downloaded from. Tests point these at `file://`.
pub struct Urls {
    pub api: String,
    pub download: String,
}

impl Default for Urls {
    fn default() -> Self {
        Urls {
            api: format!("https://api.github.com/repos/{REPO}/releases?per_page=20"),
            download: format!("https://github.com/{REPO}/releases/download"),
        }
    }
}

/// The newest release above `current` with `asset`, this system's build, if there is one.
pub async fn check_with(urls: &Urls, current: &Version, asset: &str) -> Result<Option<Release>, String> {
    let agent = format!("User-Agent: corvo-chat/{current}");
    let accept = "Accept: application/vnd.github+json";
    let body = curl(["-fsSL", "--max-time", "10", "-H", accept, "-H", &agent, &urls.api])
        .await
        .map_err(|e| format!("couldn't check for updates: {e}"))?;
    let json = String::from_utf8(body).map_err(|_| "unexpected reply from GitHub".to_string())?;
    Ok(newest_above(current, parse_releases(&json)?, asset))
}

/// An update's steps, in order: each is done between redraws, so the screen
/// can say which one is under way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Download the release and its checksum.
    Fetch,
    /// Check the checksum, unpack, and run the new binary.
    Check,
    /// Put it in place of the running one.
    Swap,
}

impl Stage {
    /// The stage after this one, if there is one.
    pub fn next(self) -> Option<Stage> {
        match self {
            Stage::Fetch => Some(Stage::Check),
            Stage::Check => Some(Stage::Swap),
            Stage::Swap => None,
        }
    }
}

/// A release downloaded into a temp folder beside the binary it replaces (so
/// the last rename is on one filesystem). Dropping it removes the folder, so
/// an update stopped between stages leaves nothing behind but the old binary.
#[derive(Debug)]
pub struct Fetched {
    dir: PathBuf,
    /// Held while the folder is in use, so another update's sweep leaves it alone.
    lock: Option<std::fs::File>,
    archive: PathBuf,
    sums: PathBuf,
}

impl Drop for Fetched {
    fn drop(&mut self) {
        // Closed first: Windows can't delete a file that's open.
        drop(self.lock.take());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Puts `release`'s `asset` in place of `target`: fetches it, checks it (and,
/// with `verify`, runs it to see it works here), then swaps it in. Any failure
/// leaves `target` as it was. The app runs the stages one at a time.
#[cfg(test)]
pub async fn install_with(
    urls: &Urls,
    release: &Release,
    asset: &str,
    target: &Path,
    verify: bool,
) -> Result<(), String> {
    let fetched = fetch(urls, release, asset, target).await?;
    check(&fetched, &release.version, verify).await?;
    swap(fetched, target)
}

/// Downloads `release`'s `asset` and its checksum into a new temp folder
/// beside `target`, first clearing what cut-short updates left there.
pub async fn fetch(urls: &Urls, release: &Release, asset: &str, target: &Path) -> Result<Fetched, String> {
    let dir = target.parent().ok_or_else(|| cant_replace(target, "it has no parent folder"))?;
    sweep_leftovers(dir);
    let tmp = dir.join(format!("{TMP_PREFIX}{}", std::process::id()));
    let lock = std::fs::create_dir(&tmp).and_then(|()| {
        let lock = std::fs::File::create(tmp.join(LOCK_FILE))?;
        lock.try_lock()?;
        Ok(lock)
    });
    let lock = match lock {
        Ok(lock) => lock,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            let reason =
                if e.kind() == ErrorKind::PermissionDenied { "permission denied".into() } else { e.to_string() };
            return Err(cant_replace(target, &reason));
        }
    };
    let fetched =
        Fetched { archive: tmp.join(asset), sums: tmp.join(format!("{asset}.sha256")), dir: tmp, lock: Some(lock) };
    let version = &release.version;
    let url = format!("{}/{}/{asset}", urls.download, release.tag);
    let failed = |e: String| format!("couldn't download v{version}: {e}");
    download(&url, &fetched.archive).await.map_err(failed)?;
    download(&format!("{url}.sha256"), &fetched.sums).await.map_err(failed)?;
    Ok(fetched)
}

/// Checks the download against its checksum, unpacks it and, with `verify`,
/// runs the new binary to see it says `version`. Then saves it to disk.
pub async fn check(fetched: &Fetched, version: &Version, verify: bool) -> Result<(), String> {
    let expected = std::fs::read_to_string(&fetched.sums).unwrap_or_default();
    let expected = expected.split_whitespace().next().unwrap_or_default().to_lowercase();
    let bytes = std::fs::read(&fetched.archive).map_err(|e| e.to_string())?;
    let actual: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    if expected != actual {
        return Err("the download didn't match its checksum; nothing changed".into());
    }

    let status = tokio::process::Command::new(tar_program())
        .kill_on_drop(true)
        .arg("-xf")
        .arg(&fetched.archive)
        .arg("-C")
        .arg(&fetched.dir)
        .status()
        .await;
    match status {
        Ok(s) if s.success() => {}
        Ok(_) => return Err("couldn't unpack the download; nothing changed".into()),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err("corvo-chat --update needs tar, which isn't installed".into());
        }
        Err(e) => return Err(format!("couldn't run tar: {e}")),
    }
    let new = fetched.dir.join(BIN_NAME);
    if !new.is_file() {
        return Err("the download didn't contain corvo-chat; nothing changed".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    if verify && !prints_version(&new, version).await {
        return Err("the new version doesn't run here; nothing changed".into());
    }
    // On disk before the swap, and the swap itself after it, in case the power goes.
    // Windows flushes only through a handle that can write.
    let synced = std::fs::OpenOptions::new().write(true).open(&new).and_then(|f| f.sync_all());
    synced.map_err(|e| format!("couldn't save the new version: {e}; nothing changed"))
}

/// Puts the checked binary in place of `target`, then removes the temp folder.
pub fn swap(fetched: Fetched, target: &Path) -> Result<(), String> {
    swap_in(&fetched.dir.join(BIN_NAME), target).map_err(|e| cant_replace(target, &e.to_string()))?;
    #[cfg(unix)]
    if let Some(dir) = target.parent() {
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(())
}

/// Whether `exe --version` prints `corvo-chat <version>` within [`VERSION_WAIT`].
async fn prints_version(exe: &Path, version: &Version) -> bool {
    let run = tokio::process::Command::new(exe).arg("--version").kill_on_drop(true).output();
    match tokio::time::timeout(VERSION_WAIT, run).await {
        Ok(Ok(out)) => out.status.success() && says_version(&String::from_utf8_lossy(&out.stdout), version),
        _ => false,
    }
}

/// Whether `--version` output says `corvo-chat <version>` on its first
/// line. Only that start counts, so a later release can add to it.
fn says_version(stdout: &str, version: &Version) -> bool {
    let mut words = stdout.lines().next().unwrap_or_default().split_whitespace();
    words.next() == Some("corvo-chat") && words.next() == Some(version.to_string().as_str())
}

/// Windows' own bsdtar reads zip; a GNU tar from Git for Windows on PATH doesn't.
fn tar_program() -> PathBuf {
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        return Path::new(&root).join("System32").join("tar.exe");
    }
    PathBuf::from("tar")
}

/// Puts `new` where `target` is. The running app keeps its open copy of the
/// old file; starting again picks up the new one.
#[cfg(not(windows))]
fn swap_in(new: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(new, target)
}

/// Windows won't replace a running exe but will rename it, so move it aside
/// first and put it back if the new one can't go in.
#[cfg(windows)]
fn swap_in(new: &Path, target: &Path) -> std::io::Result<()> {
    let old = set_aside(target)?;
    if let Err(e) = std::fs::rename(new, target) {
        return match std::fs::rename(&old, target) {
            Ok(()) => Err(e),
            Err(_) => Err(std::io::Error::other(format!(
                "{e}; the old version is at {}, rename it back to {}",
                old.display(),
                target.display()
            ))),
        };
    }
    Ok(())
}

/// Moves `exe` (which may be running) to `corvo-chat.old-<n>.exe` beside it,
/// and returns where it went. A later update's sweep removes it.
pub fn set_aside(exe: &Path) -> std::io::Result<PathBuf> {
    let old = aside_path(exe.parent().unwrap_or(Path::new(".")));
    std::fs::rename(exe, &old)?;
    Ok(old)
}

/// The first `corvo-chat.old-<n>.exe` in `dir` that's free, or can be freed.
fn aside_path(dir: &Path) -> PathBuf {
    (0..)
        .map(|n| dir.join(format!("{OLD_EXE_PREFIX}-{n}.exe")))
        .find(|path| {
            let _ = std::fs::remove_file(path);
            !path.exists()
        })
        .expect("some name is free")
}

/// The running binary's path. Unix resolves symlinks; Windows keeps the plain
/// path, since `canonicalize` there gives a `\\?\` path that curl and tar
/// and the messages would then show.
pub fn running_exe() -> std::io::Result<PathBuf> {
    if cfg!(windows) { std::env::current_exe() } else { std::env::current_exe()?.canonicalize() }
}

/// Removes temp folders left by updates that were cut short (not one another
/// update holds locked), and exes an earlier Windows update moved aside (one
/// still running can't go yet).
fn sweep_leftovers(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if name.starts_with(TMP_PREFIX) && !in_use(&path) {
            let _ = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
        }
        if name.starts_with(OLD_EXE_PREFIX) && name.ends_with(".exe") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Whether an update is working in the temp folder `tmp`: its lock is held.
fn in_use(tmp: &Path) -> bool {
    match std::fs::File::open(tmp.join(LOCK_FILE)) {
        Ok(lock) => matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
        Err(_) => false,
    }
}

/// How to reinstall by hand on this system.
fn reinstall_hint() -> String {
    if cfg!(windows) { format!("irm {INSTALL_PS1_URL} | iex") } else { format!("curl -fsSL {INSTALL_URL} | sh") }
}

fn cant_replace(target: &Path, reason: &str) -> String {
    format!("can't replace {}: {reason}. Reinstall with: {}", target.display(), reinstall_hint())
}

async fn download(url: &str, out: &Path) -> Result<(), String> {
    let args = [OsStr::new("-fsSL"), OsStr::new("--max-time"), OsStr::new("120"), OsStr::new("-o"), out.as_os_str()];
    curl(args.into_iter().chain([OsStr::new(url)])).await.map(|_| ())
}

/// Runs curl and returns what it wrote.
async fn curl<I, S>(args: I) -> Result<Vec<u8>, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = tokio::process::Command::new("curl").kill_on_drop(true).args(args).output().await.map_err(|e| {
        if e.kind() == ErrorKind::NotFound {
            "corvo-chat --update needs curl, which isn't installed".to_string()
        } else {
            format!("couldn't run curl: {e}")
        }
    })?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    Err(curl_failure(out.status.code(), &String::from_utf8_lossy(&out.stderr)))
}

/// What a failed curl means, from its exit `code` and what it wrote to stderr.
/// With `-f`, any HTTP error is exit 22 and the status is in the message.
pub fn curl_failure(code: Option<i32>, stderr: &str) -> String {
    let said = stderr.trim().trim_start_matches("curl: ").trim();
    let http = |status: &str| code == Some(22) && said.ends_with(&format!("error: {status}"));
    match code {
        Some(6 | 7) => "no connection to GitHub; check your internet and try again".into(),
        Some(28) => "GitHub didn't answer in time; try again".into(),
        _ if http("403") || http("429") => "GitHub is limiting requests from here; try again in an hour".into(),
        _ if said.is_empty() => "the download failed".into(),
        _ => said.to_string(),
    }
}

/// A fake release in a temp folder, served through `file://`.
#[cfg(test)]
pub mod testing {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::fs;

    /// A `file://` URL for `path`, which curl reads like a download.
    pub fn file_url(path: &Path) -> String {
        let path = path.display().to_string().replace('\\', "/");
        if path.starts_with('/') { format!("file://{path}") } else { format!("file:///{path}") }
    }

    /// What the "binary" in a fake release does when run.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Binary {
        /// Prints `corvo-chat 9.0.0`, like the real one (a shell script, so Unix only).
        Works,
        /// Fails when run.
        #[cfg_attr(not(unix), allow(dead_code))]
        Broken,
        /// The text `new`: for systems where it isn't run.
        Text,
    }

    /// A release v9.0.0 in `site` that works where it's run, tagged `v9.0.0`.
    /// Returns the URLs and the asset's name.
    pub fn publish(site: &Path, checksum_ok: bool) -> (Urls, String) {
        let binary = if cfg!(unix) { Binary::Works } else { Binary::Text };
        publish_as(site, "v9.0.0", binary, checksum_ok)
    }

    /// A release v9.0.0 in `site` under `tag`, holding `binary`.
    pub fn publish_as(site: &Path, tag: &str, binary: Binary, checksum_ok: bool) -> (Urls, String) {
        let asset = "corvo-chat-test.tar.gz".to_string();
        let staging = site.join("staging");
        fs::create_dir_all(&staging).unwrap();
        let contents = match binary {
            Binary::Works => "#!/bin/sh\necho 'corvo-chat 9.0.0'\n",
            Binary::Broken => "#!/bin/sh\nexit 1\n",
            Binary::Text => "new",
        };
        fs::write(staging.join(BIN_NAME), contents).unwrap();
        let dir = site.join("download").join(tag);
        fs::create_dir_all(&dir).unwrap();
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(dir.join(&asset))
            .arg("-C")
            .arg(&staging)
            .arg(BIN_NAME)
            .status()
            .unwrap();
        assert!(status.success());
        let hash = Sha256::digest(fs::read(dir.join(&asset)).unwrap());
        let mut hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        if !checksum_ok {
            hex = "0".repeat(64);
        }
        fs::write(dir.join(format!("{asset}.sha256")), format!("{hex}  {asset}\n")).unwrap();
        let json = format!(
            r#"[{{"tag_name": "{tag}", "draft": false, "prerelease": false, "assets": [{{"name": "{asset}", "size": 1}}, {{"name": "{asset}.sha256", "size": 1}}]}}]"#
        );
        fs::write(site.join("releases.json"), json).unwrap();
        let urls = Urls { api: file_url(&site.join("releases.json")), download: file_url(&site.join("download")) };
        (urls, asset)
    }

    /// Whether the new binary is run before the swap: where the fake one can run.
    pub const VERIFY: bool = cfg!(unix);
}

#[cfg(test)]
mod tests {
    use super::testing::{Binary, VERIFY, file_url, publish, publish_as};
    use super::*;
    use std::fs;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn pre_releases_come_before_their_release() {
        let order = ["0.0.9", "0.1.0-rc.1", "0.1.0-rc.2", "0.1.0-rc.10", "0.1.0", "0.1.1", "0.10.0"];
        for pair in order.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
    }

    const RELEASES: &str = r#"[
        {"tag_name": "v0.1.0-rc.2", "draft": false, "prerelease": true,
         "assets": [{"name": "corvo-chat-aarch64-apple-darwin.tar.gz", "size": 10},
                    {"name": "corvo-chat-aarch64-apple-darwin.tar.gz.sha256", "size": 1}]},
        {"tag_name": "v0.2.0", "draft": true, "prerelease": false, "assets": []},
        {"tag_name": "nightly", "draft": false, "prerelease": true, "assets": []},
        {"tag_name": "v0.1.0-rc.1", "draft": false, "prerelease": true, "assets": []}
    ]"#;

    #[test]
    fn releases_are_read_newest_first_without_drafts() {
        let releases = parse_releases(RELEASES).unwrap();
        let versions: Vec<String> = releases.iter().map(|r| r.version.to_string()).collect();
        assert_eq!(versions, vec!["0.1.0-rc.2", "0.1.0-rc.1"], "drafts and odd tags are skipped");
        assert_eq!(releases[0].assets[0].size, 10);
        assert!(parse_releases("{\"message\": \"rate limited\"}").is_err());
    }

    #[test]
    fn only_a_newer_release_is_offered() {
        let releases = parse_releases(RELEASES).unwrap();
        let mac = "corvo-chat-aarch64-apple-darwin.tar.gz";
        assert_eq!(newest_above(&v("0.1.0-rc.1"), releases.clone(), mac).map(|r| r.version), Some(v("0.1.0-rc.2")));
        assert_eq!(newest_above(&v("0.1.0-rc.2"), releases.clone(), mac), None);
        assert_eq!(newest_above(&v("0.1.0"), releases, mac), None, "a full release is newer than its candidates");
    }

    #[test]
    fn pre_releases_are_offered_only_to_a_pre_release() {
        let json = r#"[
            {"tag_name": "v0.2.0-rc.1", "prerelease": true, "assets": [{"name": "mac.tar.gz", "size": 1}, {"name": "mac.tar.gz.sha256", "size": 1}]},
            {"tag_name": "v0.1.1", "assets": [{"name": "mac.tar.gz", "size": 1}, {"name": "mac.tar.gz.sha256", "size": 1}]}
        ]"#;
        let releases = parse_releases(json).unwrap();
        assert_eq!(newest_above(&v("0.1.0"), releases.clone(), "mac.tar.gz").map(|r| r.version), Some(v("0.1.1")));
        assert_eq!(newest_above(&v("0.1.1"), releases.clone(), "mac.tar.gz"), None);
        assert_eq!(newest_above(&v("0.1.0-rc.3"), releases, "mac.tar.gz").map(|r| r.version), Some(v("0.2.0-rc.1")));
    }

    #[test]
    fn each_system_gets_its_build() {
        assert_eq!(asset_name("macos", "aarch64").as_deref(), Some("corvo-chat-aarch64-apple-darwin.tar.gz"));
        assert_eq!(asset_name("macos", "x86_64").as_deref(), Some("corvo-chat-x86_64-apple-darwin.tar.gz"));
        assert_eq!(asset_name("linux", "aarch64").as_deref(), Some("corvo-chat-aarch64-unknown-linux-musl.tar.gz"));
        assert_eq!(asset_name("windows", "x86_64").as_deref(), Some("corvo-chat-x86_64-pc-windows-msvc.zip"));
        assert_eq!(asset_name("windows", "aarch64"), None);
        assert_eq!(asset_name("freebsd", "x86_64"), None);
    }

    #[tokio::test]
    async fn a_newer_release_is_found_and_put_in_place() {
        let (site, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (urls, asset) = publish(site.path(), true);
        let target = bin.path().join(BIN_NAME);
        fs::write(&target, "old").unwrap();
        let found = check_with(&urls, &v("0.1.0-rc.1"), &asset).await.unwrap().expect("v9.0.0 is newer");
        assert_eq!(found.version, v("9.0.0"));
        assert_eq!(check_with(&urls, &v("9.0.0"), &asset).await.unwrap(), None);
        let new = fs::read_to_string(site.path().join("staging").join(BIN_NAME)).unwrap();
        install_with(&urls, &found, &asset, &target, VERIFY).await.unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), new);
        let mut left: Vec<String> =
            fs::read_dir(bin.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        // Windows can't delete a running exe, so the old one waits beside it
        // until the next update clears it.
        let expected = if cfg!(windows) { vec![BIN_NAME, "corvo-chat.old-0.exe"] } else { vec![BIN_NAME] };
        assert_eq!(left, expected, "no temp files left beside it");
        if cfg!(windows) {
            assert_eq!(fs::read_to_string(bin.path().join("corvo-chat.old-0.exe")).unwrap(), "old");
        }
    }

    #[tokio::test]
    async fn a_bad_checksum_changes_nothing() {
        let (site, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (urls, asset) = publish(site.path(), false);
        let target = bin.path().join(BIN_NAME);
        fs::write(&target, "old").unwrap();
        let found = check_with(&urls, &v("0.1.0"), &asset).await.unwrap().unwrap();
        let err = install_with(&urls, &found, &asset, &target, VERIFY).await.unwrap_err();
        assert!(err.contains("nothing changed"), "{err}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
    }

    #[tokio::test]
    async fn a_release_without_this_systems_checksum_is_not_offered() {
        let site = tempfile::tempdir().unwrap();
        let (urls, asset) = publish(site.path(), true);
        assert!(check_with(&urls, &v("0.1.0"), &asset).await.unwrap().is_some(), "offered with its checksum");
        let json = format!(r#"[{{"tag_name": "v9.0.0", "assets": [{{"name": "{asset}", "size": 1}}]}}]"#);
        fs::write(site.path().join("releases.json"), json).unwrap();
        assert_eq!(check_with(&urls, &v("0.1.0"), &asset).await.unwrap(), None, "no .sha256, no offer");
    }

    #[tokio::test]
    async fn an_unreachable_server_says_so() {
        let site = tempfile::tempdir().unwrap();
        let urls = Urls { api: file_url(&site.path().join("missing.json")), download: String::new() };
        assert!(check_with(&urls, &v("0.1.0"), "any").await.unwrap_err().contains("couldn't"));
    }

    #[test]
    fn only_releases_with_this_build_are_offered() {
        let json = r#"[
            {"tag_name": "v0.3.0", "assets": [{"name": "corvo-chat-x86_64-pc-windows-msvc.zip", "size": 1},
                {"name": "corvo-chat-x86_64-pc-windows-msvc.zip.sha256", "size": 1}]},
            {"tag_name": "v0.2.0", "assets": [{"name": "corvo-chat-aarch64-apple-darwin.tar.gz", "size": 1},
                {"name": "corvo-chat-aarch64-apple-darwin.tar.gz.sha256", "size": 1}]}
        ]"#;
        let releases = parse_releases(json).unwrap();
        let mac = "corvo-chat-aarch64-apple-darwin.tar.gz";
        assert_eq!(newest_above(&v("0.1.0"), releases.clone(), mac).map(|r| r.version), Some(v("0.2.0")));
        assert_eq!(newest_above(&v("0.2.0"), releases, mac), None, "0.3.0 has no build for this system");
        assert_eq!(no_build("freebsd", "x86_64"), "no release build for freebsd/x86_64; build from source");
    }

    #[test]
    fn curl_failures_are_explained() {
        let limited = "GitHub is limiting requests from here; try again in an hour";
        assert_eq!(curl_failure(Some(22), "curl: (22) The requested URL returned error: 403"), limited);
        assert_eq!(curl_failure(Some(22), "curl: (22) The requested URL returned error: 429"), limited);
        assert_eq!(curl_failure(Some(28), "curl: (28) Operation timed out"), "GitHub didn't answer in time; try again");
        assert_eq!(curl_failure(Some(6), ""), "no connection to GitHub; check your internet and try again");
        assert_eq!(
            curl_failure(Some(22), "curl: (22) The requested URL returned error: 404"),
            "(22) The requested URL returned error: 404"
        );
        assert_eq!(curl_failure(None, ""), "the download failed");
    }

    #[tokio::test]
    async fn downloads_use_the_tag() {
        let (site, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let binary = if VERIFY { Binary::Works } else { Binary::Text };
        let (urls, asset) = publish_as(site.path(), "9.0.0", binary, true);
        let target = bin.path().join(BIN_NAME);
        fs::write(&target, "old").unwrap();
        let found = check_with(&urls, &v("0.1.0"), &asset).await.unwrap().unwrap();
        assert_eq!(found.tag, "9.0.0");
        install_with(&urls, &found, &asset, &target, VERIFY).await.unwrap();
        assert_ne!(fs::read_to_string(&target).unwrap(), "old", "found under 9.0.0, not v9.0.0");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_new_binary_that_does_not_run_changes_nothing() {
        let (site, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (urls, asset) = publish_as(site.path(), "v9.0.0", Binary::Broken, true);
        let target = bin.path().join(BIN_NAME);
        fs::write(&target, "old").unwrap();
        let found = check_with(&urls, &v("0.1.0"), &asset).await.unwrap().unwrap();
        let err = install_with(&urls, &found, &asset, &target, true).await.unwrap_err();
        assert_eq!(err, "the new version doesn't run here; nothing changed");
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
    }

    #[tokio::test]
    async fn the_stages_install_like_install_with() {
        let (site, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (urls, asset) = publish(site.path(), true);
        let target = bin.path().join(BIN_NAME);
        fs::write(&target, "old").unwrap();
        let found = check_with(&urls, &v("0.1.0"), &asset).await.unwrap().unwrap();
        let fetched = fetch(&urls, &found, &asset, &target).await.unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "old", "fetching changes nothing");
        check(&fetched, &found.version, VERIFY).await.unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "old", "nor does checking");
        swap(fetched, &target).unwrap();
        let new = fs::read_to_string(site.path().join("staging").join(BIN_NAME)).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), new);
        let temp = fs::read_dir(bin.path())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with(TMP_PREFIX));
        assert!(!temp, "the temp folder is gone");
    }

    #[tokio::test]
    async fn dropping_fetched_removes_the_temp_folder() {
        let (site, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (urls, asset) = publish(site.path(), true);
        let target = bin.path().join(BIN_NAME);
        fs::write(&target, "old").unwrap();
        let found = check_with(&urls, &v("0.1.0"), &asset).await.unwrap().unwrap();
        let fetched = fetch(&urls, &found, &asset, &target).await.unwrap();
        let temp = fetched.dir.clone();
        assert!(temp.join(&asset).is_file(), "downloaded into it");
        drop(fetched);
        assert!(!temp.exists());
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
    }

    #[test]
    fn a_locked_temp_folder_is_left_alone() {
        let bin = tempfile::tempdir().unwrap();
        let (busy, done, old) = (
            bin.path().join(format!("{TMP_PREFIX}1")),
            bin.path().join(format!("{TMP_PREFIX}2")),
            bin.path().join(format!("{TMP_PREFIX}3")),
        );
        for dir in [&busy, &done, &old] {
            fs::create_dir(dir).unwrap();
        }
        let lock = fs::File::create(busy.join(LOCK_FILE)).unwrap();
        lock.try_lock().unwrap();
        fs::File::create(done.join(LOCK_FILE)).unwrap();
        sweep_leftovers(bin.path());
        assert!(busy.exists(), "another update is using it");
        assert!(!done.exists() && !old.exists(), "left by updates that were cut short");
    }

    #[test]
    fn the_new_version_counts_by_its_first_line() {
        let version = v("9.0.0");
        assert!(says_version("corvo-chat 9.0.0\n", &version));
        assert!(says_version("corvo-chat 9.0.0 (abc1234 2027-01-02)\nbuilt with love\n", &version), "more can follow");
        assert!(!says_version("corvo-chat 9.0.1\n", &version));
        assert!(!says_version("corvo-chat 9.0.0-rc.1\n", &version));
        assert!(!says_version("something-else 9.0.0\n", &version));
        assert!(!says_version("", &version));
    }

    #[test]
    fn updates_come_from_the_clients_repo() {
        assert_eq!(REPO, "Abinsanth/corvo-chat-client");
        let urls = Urls::default();
        assert!(urls.api.contains("repos/Abinsanth/corvo-chat-client/"), "{}", urls.api);
        assert!(urls.download.contains("Abinsanth/corvo-chat-client/releases/download"), "{}", urls.download);
        assert!(INSTALL_URL.contains("Abinsanth/corvo-chat-client/release/"));
        assert!(INSTALL_PS1_URL.contains("Abinsanth/corvo-chat-client/release/"));
    }

    #[test]
    fn built_from_source() {
        for path in [
            "/src/corvo/target/debug/corvo-chat",
            "/src/corvo/target/release/corvo-chat",
            "/src/corvo/target/x86_64-unknown-linux-musl/release/corvo-chat",
            "C:/src/corvo/target/debug/corvo-chat.exe",
        ] {
            assert!(is_built_from_source(Path::new(path)), "{path}");
        }
        for path in [
            "/home/ana/.local/bin/corvo-chat",
            "/opt/target/corvo-chat",
            "/home/ana/release/corvo-chat",
            "/usr/local/bin/corvo-chat",
        ] {
            assert!(!is_built_from_source(Path::new(path)), "{path}");
        }
        assert_eq!(FROM_SOURCE, "built from source; update with git pull && cargo build");
    }
}
