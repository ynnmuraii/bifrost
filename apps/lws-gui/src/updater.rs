//! GitHub release checks and verified, same-directory binary replacement.
//! This module is GUI-only: the agent has no HTTP dependency.
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use semver::Version;
use serde::Deserialize;
use ring::digest::{digest, SHA256};

pub const RELEASES: &str = "https://github.com/ynnmuraii/bifrost/releases";
pub const LATEST: &str = "https://api.github.com/repos/ynnmuraii/bifrost/releases/latest";
const CAP: u64 = 200 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Default)]
pub enum State {
    #[default]
    Idle,
    Checking,
    Current,
    Available(Release),
    Downloading {
        received: u64,
        total: Option<u64>,
    },
    Restarting,
    Error(String),
}
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Check,
    Startup(bool),
    Later,
    Skip,
    Confirm,
    Install,
    Cancel,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    pub html_url: String,
    pub assets: Vec<Asset>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
}
impl Release {
    pub fn version(&self) -> Result<Version, String> {
        parse_version(&self.tag_name)
    }
    pub fn asset(&self, platform: &str) -> Result<&Asset, String> {
        let version = self.version()?;
        let suffix = match platform {
            "windows" => "windows-x86_64.exe",
            "linux" => "linux-x86_64.tar.gz",
            _ => return Err("Unsupported update platform".into()),
        };
        let name = format!("bifrost-{version}-{suffix}");
        self.assets
            .iter()
            .find(|a| a.name == name)
            .ok_or_else(|| format!("Missing asset: {name}"))
    }
}
pub fn parse_version(tag: &str) -> Result<Version, String> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).map_err(|e| e.to_string())
}
pub fn offer(
    release: &Release,
    current: &str,
    skipped: Option<[u64; 3]>,
    manual: bool,
) -> Result<bool, String> {
    let version = release.version()?;
    Ok(!release.draft
        && !release.prerelease
        && version.pre.is_empty()
        && version.cmp_precedence(&parse_version(current)?).is_gt()
        && (manual || skipped != Some([version.major, version.minor, version.patch])))
}
/// Exact host names, never suffix matching or userinfo tricks.
pub fn allowed_asset_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|u| {
        u.scheme() == "https"
            && u.username().is_empty()
            && u.password().is_none()
            && u.port_or_known_default() == Some(443)
            && matches!(
                u.host_str(),
                Some(
                    "github.com"
                        | "objects.githubusercontent.com"
                        | "release-assets.githubusercontent.com"
                )
            )
    })
}
fn client(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .user_agent(format!("bifrost/{}", env!("CARGO_PKG_VERSION")))
        // Inspect every redirect ourselves, before sending a request to its destination.
        .max_redirects(0)
        .max_redirects_will_error(false)
        .timeout_global(Some(timeout))
        .build()
        .into()
}
pub fn check() -> Result<Release, String> {
    let mut response = client(Duration::from_secs(10))
        .get(LATEST)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err("Release metadata is too large".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}
fn download(
    value: &str,
    cap: u64,
    progress: &mut impl FnMut(u64, Option<u64>),
) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut location = value.to_owned();
    for _ in 0..6 {
        if !allowed_asset_url(&location) {
            return Err(format!("Untrusted asset URL: {location}"));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Download timed out".into());
        }
        let mut response = client(remaining)
            .get(&location)
            .call()
            .map_err(|e| e.to_string())?;
        if response.status().is_redirection() {
            let next = response
                .headers()
                .get("location")
                .and_then(|s| s.to_str().ok())
                .ok_or("Redirect without Location")?;
            location = url::Url::parse(&location)
                .map_err(|e| e.to_string())?
                .join(next)
                .map_err(|e| e.to_string())?
                .into();
            continue;
        }
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let size = response
            .headers()
            .get("content-length")
            .and_then(|s| s.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        if size.is_some_and(|size| size > cap) {
            return Err("Download is too large".into());
        }
        let mut bytes = Vec::new();
        let mut reader = response.body_mut().as_reader();
        let mut buffer = [0; 64 * 1024];
        loop {
            let n = reader.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            if bytes.len() as u64 + n as u64 > cap {
                return Err("Download is too large".into());
            }
            bytes.extend_from_slice(&buffer[..n]);
            progress(bytes.len() as u64, size);
        }
        return Ok(bytes);
    }
    Err("Too many redirects".into())
}
pub fn expected_hash(sums: &str, filename: &str) -> Result<[u8; 32], String> {
    let mut found = None;
    for line in sums.lines() {
        let Some((hash, name)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if name.trim_start().trim_start_matches('*') != filename {
            continue;
        }
        if found.is_some() || hash.len() != 64 || !hash.is_ascii() {
            return Err("Invalid or duplicate SHA256SUMS entry".into());
        }
        let mut digest = [0; 32];
        for (index, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hash[index * 2..index * 2 + 2], 16)
                .map_err(|_| "Invalid SHA256SUMS hash")?;
        }
        found = Some(digest);
    }
    found.ok_or_else(|| format!("Missing SHA256SUMS entry: {filename}"))
}
pub fn verify(bytes: &[u8], sums: &str, filename: &str) -> Result<(), String> {
    let expected = expected_hash(sums, filename)?;
    if digest(&SHA256, bytes).as_ref() != expected {
        return Err("SHA256 mismatch; nothing was replaced".into());
    }
    Ok(())
}
/// Downloads and authenticates the archive before extracting a single bounded regular file.
pub fn prepare(
    release: &Release,
    progress: &mut impl FnMut(u64, Option<u64>),
) -> Result<Vec<u8>, String> {
    if !cfg!(target_arch = "x86_64") {
        return Err("Unsupported update architecture".into());
    }
    let asset = release.asset(std::env::consts::OS)?;
    let sums = release
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS.txt")
        .ok_or("Missing SHA256SUMS.txt")?;
    let sums = download(&sums.browser_download_url, 1024 * 1024, &mut |_, _| {})?;
    let sums = std::str::from_utf8(&sums).map_err(|e| e.to_string())?;
    let bytes = download(&asset.browser_download_url, CAP, progress)?;
    verify(&bytes, sums, &asset.name)?;
    if cfg!(windows) {
        return Ok(bytes);
    }
    extract_linux(&bytes)
}
fn extract_linux(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let mut binary = None;
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?;
        if path.strip_prefix(".").unwrap_or(&path) != Path::new("bifrost") {
            continue;
        }
        if !entry.header().entry_type().is_file() || entry.size() > CAP || binary.is_some() {
            return Err("Invalid bifrost archive entry".into());
        }
        let mut data = Vec::new();
        entry
            .by_ref()
            .take(CAP + 1)
            .read_to_end(&mut data)
            .map_err(|e| e.to_string())?;
        if data.len() as u64 > CAP {
            return Err("Extracted binary is too large".into());
        }
        binary = Some(data);
    }
    binary.ok_or_else(|| "Archive has no bifrost binary".into())
}
pub fn paths(target: &Path) -> (PathBuf, PathBuf) {
    (
        target.with_extension(if cfg!(windows) { "exe.new" } else { "new" }),
        target.with_extension(if cfg!(windows) { "exe.old" } else { "old" }),
    )
}
pub fn clean_old(target: &Path) {
    let _ = fs::remove_file(paths(target).1);
}
/// Canonicalize at the call site: a symlink install updates its target, not the link.
pub fn replace(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let (next, old) = paths(target);
    // Remove only our known staging path; create_new rejects a concurrent/symlink writer.
    match fs::remove_file(&next) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&next)?;
        file.write_all(bytes)?;
        file.set_permissions(fs::metadata(target)?.permissions())?;
        file.sync_all()?;
        drop(file);
        match fs::remove_file(&old) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        #[cfg(windows)]
        {
            fs::rename(target, &old)?;
            if let Err(error) = fs::rename(&next, target) {
                let _ = fs::rename(&old, target);
                return Err(error);
            }
        }
        #[cfg(not(windows))]
        {
            fs::copy(target, &old)?;
            fs::File::open(&old)?.sync_all()?;
            fs::rename(&next, target)?;
            if let Some(parent) = target.parent() {
                fs::File::open(parent)?.sync_all()?;
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&next);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn release() -> Release {
        serde_json::from_str(include_str!("../tests/fixtures/latest-release.json")).unwrap()
    }
    #[test]
    fn release_decisions() {
        let r = release();
        assert_eq!(r.name.as_deref(), Some("Bifrost 0.1.3"));
        assert!(offer(&r, "0.1.2", None, false).unwrap());
        assert!(!offer(&r, "0.1.3", None, false).unwrap());
        assert!(!offer(&r, "0.1.4", None, false).unwrap());
        assert!(!offer(&r, "0.1.3+dev", None, false).unwrap());
        assert!(offer(&r, "0.1.3-dev", None, false).unwrap());
        assert!(!offer(&r, "0.1.2", Some([0, 1, 3]), false).unwrap());
        assert!(offer(&r, "0.1.2", Some([0, 1, 3]), true).unwrap());
        assert_eq!(
            parse_version("v1.2.3").unwrap(),
            parse_version("1.2.3").unwrap()
        );
        assert!(parse_version("garbage").is_err());
        assert!(r.asset("windows").unwrap().name.ends_with(".exe"));
        assert!(r.asset("linux").unwrap().name.ends_with(".tar.gz"));
        assert!(r.asset("macos").is_err());
    }
    #[test]
    fn extracts_release_tar_layout_without_running_scripts() {
        let mut tar = tar::Builder::new(Vec::new());
        for (name, data) in [
            ("./bifrost", b"binary".as_slice()),
            ("./install.sh", b"do not run".as_slice()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, name, data).unwrap();
        }
        let tar = tar.into_inner().unwrap();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&tar).unwrap();
        assert_eq!(extract_linux(&gzip.finish().unwrap()).unwrap(), b"binary");
    }
    #[test]
    fn checksum_decisions() {
        let sums = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  bifrost.exe\n";
        assert!(verify(b"", sums, "bifrost.exe").is_ok());
        assert!(verify(b"modified", sums, "bifrost.exe").is_err());
        assert!(verify(b"", sums, "missing").is_err());
        assert!(expected_hash("not-a-hash  bifrost.exe", "bifrost.exe").is_err());
    }
    #[test]
    fn url_boundary() {
        for good in [
            "https://github.com/a",
            "https://objects.githubusercontent.com/a",
            "https://release-assets.githubusercontent.com/a",
        ] {
            assert!(allowed_asset_url(good));
        }
        for bad in [
            "http://github.com/a",
            "https://github.com.evil/a",
            "https://github.com@evil/a",
            "https://evil@github.com/a",
            "file:///a",
            "https://github.com:444/a",
        ] {
            assert!(!allowed_asset_url(bad));
        }
    }
    #[test]
    fn same_directory_replace_and_cleanup() {
        let dir = std::env::temp_dir().join(format!("bifrost-replace-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join(if cfg!(windows) {
            "bifrost.exe"
        } else {
            "bifrost"
        });
        fs::write(&target, b"old").unwrap();
        fs::write(paths(&target).0, b"stale").unwrap();
        replace(&target, b"new").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(fs::read(paths(&target).1).unwrap(), b"old");
        assert!(!paths(&target).0.exists());
        clean_old(&target);
        assert!(!paths(&target).1.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
