//! Updating orbis itself from its GitHub releases.
//!
//! A release carries `orbis-x86_64-linux.tar.gz` (a folder holding the
//! `orbis` binary) and a `.sha256` file beside it. Updating downloads both,
//! checks the hash, runs the new binary once to make sure it starts, and
//! renames it over the running one; the next start uses it.

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cancel::Cancel;
use crate::download;
use crate::metadata::DownloadHash;
use crate::paths::Paths;

/// Where the latest release is described.
pub const LATEST_URL: &str = "https://api.github.com/repos/grunclepug/orbis/releases/latest";
/// The release archive for this platform.
pub const ASSET: &str = "orbis-x86_64-linux.tar.gz";

/// A published release.
#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    /// The tag, for example `v0.14.0`.
    pub tag_name: String,
    /// The release page.
    pub html_url: String,
    /// Release notes (Markdown).
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// A file attached to a release.
#[derive(Clone, Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

impl Release {
    /// The version without the leading `v`.
    pub fn version(&self) -> &str {
        self.tag_name.trim_start_matches('v')
    }

    /// Whether this release is newer than the running orbis.
    pub fn is_newer(&self) -> bool {
        newer(self.version(), env!("CARGO_PKG_VERSION"))
    }

    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// `MAJOR.MINOR.PATCH` as numbers; anything else (a pre-release suffix,
/// a missing part) doesn't parse.
fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().trim_start_matches('v').split('.');
    let n = |p: Option<&str>| p?.parse::<u64>().ok();
    let v = (n(it.next())?, n(it.next())?, n(it.next())?);
    it.next().is_none().then_some(v)
}

/// Whether version `a` is newer than `b`.
pub fn newer(a: &str, b: &str) -> bool {
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// Fetches the latest release from `url` (normally `LATEST_URL`). The
/// answer is cached with its ETag, so asking again costs GitHub nothing
/// when nothing changed.
pub fn latest(paths: &Paths, url: &str, cancel: &Cancel) -> Result<Release> {
    let dir = paths.cache.join("update");
    let json = dir.join("latest.json");
    let etag = dir.join("latest.etag");
    if !json.is_file() {
        // An ETag without the answer it belongs to would only get a 304.
        let _ = fs::remove_file(&etag);
    }
    download::fetch(&[url.to_owned()], &json, Some(&etag), &mut |_| {}, cancel)
        .context("ask GitHub for the latest orbis release")?;
    let text = fs::read_to_string(&json).with_context(|| format!("read {}", json.display()))?;
    serde_json::from_str(&text).context("read the latest release")
}

/// Why orbis can't replace the binary at `exe` itself, if it can't.
pub fn blocker(exe: &Path) -> Option<String> {
    let dir = exe.parent()?;
    let parts: Vec<_> = exe.components().map(|c| c.as_os_str()).collect();
    if parts
        .windows(2)
        .any(|w| w[0] == "target" && (w[1] == "release" || w[1] == "debug"))
    {
        return Some(
            "this orbis was built from source; update it with git pull and make build".into(),
        );
    }
    let probe = dir.join(format!(".orbis-update-{}", std::process::id()));
    match fs::File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            None
        }
        Err(_) => Some(format!(
            "orbis can't write to {} (installed by a package manager or with sudo); update it the same way you installed it",
            dir.display()
        )),
    }
}

/// The running binary.
pub fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("find the running orbis")?;
    Ok(fs::canonicalize(&exe).unwrap_or(exe))
}

/// Downloads `release`, checks it, and puts its binary in place of `exe`.
pub fn install(paths: &Paths, release: &Release, exe: &Path, cancel: &Cancel) -> Result<()> {
    if let Some(why) = blocker(exe) {
        bail!("{why}");
    }
    let archive = release
        .asset(ASSET)
        .with_context(|| format!("release {} has no {ASSET}", release.tag_name))?;
    let sum = release
        .asset(&format!("{ASSET}.sha256"))
        .with_context(|| format!("release {} has no {ASSET}.sha256", release.tag_name))?;
    let dir = paths.cache.join("update");
    let tarball = dir.join(format!("orbis-{}.tar.gz", release.version()));
    let sumfile = dir.join(format!("orbis-{}.sha256", release.version()));
    let _ = fs::remove_file(&tarball);
    download::fetch(
        std::slice::from_ref(&sum.browser_download_url),
        &sumfile,
        None,
        &mut |_| {},
        cancel,
    )
    .context("download the checksum")?;
    download::fetch(
        std::slice::from_ref(&archive.browser_download_url),
        &tarball,
        None,
        &mut |_| {},
        cancel,
    )
    .context("download the release")?;
    let want = fs::read_to_string(&sumfile)
        .with_context(|| format!("read {}", sumfile.display()))?
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .context("empty checksum file")?;
    download::verify(
        &tarball,
        Some(&DownloadHash {
            sha1: None,
            sha256: Some(want),
        }),
    )?;

    let parent = exe.parent().context("orbis has no parent directory")?;
    let new = parent.join(".orbis.new");
    let result = (|| -> Result<()> {
        extract_binary(&tarball, &new)?;
        check_runs(&new, release.version())?;
        fs::rename(&new, exe).with_context(|| format!("replace {}", exe.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&new);
    }
    let _ = fs::remove_file(&tarball);
    let _ = fs::remove_file(&sumfile);
    result
}

/// Writes the `orbis` file from the release archive to `dest`.
fn extract_binary(tarball: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(tarball).with_context(|| format!("open {}", tarball.display()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in archive.entries().context("read the release archive")? {
        let mut entry = entry.context("read the release archive")?;
        let path = entry.path().context("read the release archive")?;
        if !entry.header().entry_type().is_file() || path.file_name() != Some("orbis".as_ref()) {
            continue;
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .context("unpack the orbis binary")?;
        fs::write(dest, &bytes).with_context(|| format!("write {}", dest.display()))?;
        fs::set_permissions(dest, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("make {} executable", dest.display()))?;
        return Ok(());
    }
    bail!("the release archive holds no orbis binary")
}

/// Runs the new binary with `--version` and checks it reports `version`.
fn check_runs(bin: &Path, version: &str) -> Result<()> {
    let out = Command::new(bin)
        .arg("--version")
        .output()
        .context("run the new orbis")?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || text.split_whitespace().nth(1) != Some(version) {
        bail!(
            "the downloaded orbis doesn't run here (it said {:?})",
            text.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn compares_versions() {
        let cases = [
            ("0.14.0", "0.13.4", true),
            ("v0.13.10", "0.13.9", true),
            ("1.0.0", "0.99.99", true),
            ("0.13.4", "0.13.4", false),
            ("0.13.3", "0.13.4", false),
            ("0.14.0-rc1", "0.13.4", false),
            ("garbage", "0.1.0", false),
        ];
        for (a, b, want) in cases {
            assert_eq!(newer(a, b), want, "{a} vs {b}");
        }
    }

    #[test]
    fn a_source_build_is_not_replaced() {
        assert!(blocker(Path::new("/src/orbis/target/release/orbis")).is_some());
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(blocker(&tmp.path().join("orbis")), None);
    }

    /// A release on disk: a tarball holding a fake `orbis` script that
    /// reports `version`, its checksum, and the release description.
    fn fake_release(dir: &Path, version: &str, corrupt: bool) -> String {
        let script = format!("#!/bin/sh\necho \"orbis {version}\"\n");
        let tarball = dir.join(ASSET);
        {
            let gz = flate2::write::GzEncoder::new(
                fs::File::create(&tarball).expect("create"),
                flate2::Compression::fast(),
            );
            let mut tar = tar::Builder::new(gz);
            let mut h = tar::Header::new_gnu();
            h.set_size(script.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            tar.append_data(&mut h, "orbis-x86_64-linux/orbis", script.as_bytes())
                .expect("append");
            tar.into_inner().expect("tar").finish().expect("gz");
        }
        let mut sum = {
            use sha2::{Digest, Sha256};
            download::hex(&Sha256::digest(fs::read(&tarball).expect("read")))
        };
        if corrupt {
            sum = "0".repeat(64);
        }
        let mut f = fs::File::create(dir.join(format!("{ASSET}.sha256"))).expect("create");
        writeln!(f, "{sum}  {ASSET}").expect("write");
        let url = |n: &str| format!("file://{}", dir.join(n).display());
        let json = serde_json::json!({
            "tag_name": format!("v{version}"),
            "html_url": "https://example.invalid/release",
            "body": "notes",
            "assets": [
                {"name": ASSET, "browser_download_url": url(ASSET)},
                {"name": format!("{ASSET}.sha256"), "browser_download_url": url(&format!("{ASSET}.sha256"))},
            ],
        });
        fs::write(dir.join("latest.json"), json.to_string()).expect("write");
        url("latest.json")
    }

    #[test]
    fn installs_a_checked_release_over_the_binary() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(Some(&tmp.path().join("orbis"))).expect("paths");
        let server = tmp.path().join("server");
        fs::create_dir_all(&server).expect("mkdir");
        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).expect("mkdir");
        let exe = bin.join("orbis");
        fs::write(&exe, "old").expect("write");
        let cancel = Cancel::default();

        let url = fake_release(&server, "99.0.0", true);
        let release = latest(&paths, &url, &cancel).expect("latest");
        assert_eq!(release.version(), "99.0.0");
        assert!(release.is_newer());
        let err = install(&paths, &release, &exe, &cancel).expect_err("bad checksum");
        assert!(format!("{err:#}").contains("SHA-256"), "{err:#}");
        assert_eq!(fs::read_to_string(&exe).expect("read"), "old", "untouched");

        let url = fake_release(&server, "99.0.0", false);
        let release = latest(&paths, &url, &cancel).expect("latest");
        install(&paths, &release, &exe, &cancel).expect("install");
        let out = Command::new(&exe).arg("--version").output().expect("run");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "orbis 99.0.0");
        assert!(!bin.join(".orbis.new").exists());
    }

    #[test]
    fn a_binary_that_reports_another_version_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(Some(&tmp.path().join("orbis"))).expect("paths");
        let server = tmp.path().join("server");
        fs::create_dir_all(&server).expect("mkdir");
        let exe = tmp.path().join("orbis-bin");
        fs::write(&exe, "old").expect("write");
        let cancel = Cancel::default();
        let url = fake_release(&server, "99.0.0", false);
        let mut release = latest(&paths, &url, &cancel).expect("latest");
        release.tag_name = "v98.0.0".into();
        install(&paths, &release, &exe, &cancel).expect_err("version mismatch");
        assert_eq!(fs::read_to_string(&exe).expect("read"), "old");
    }
}
