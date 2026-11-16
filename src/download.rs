//! Downloads through the system `curl` binary, with hash checks and a cache.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::metadata::{DownloadHash, Module};
use crate::paths::slug;

/// The User-Agent sent with every request.
pub fn user_agent() -> String {
    format!(
        "orbis/{} (+https://github.com/grunclepug/orbis)",
        env!("CARGO_PKG_VERSION")
    )
}

/// Result of a conditional fetch.
#[derive(Debug, PartialEq, Eq)]
pub enum Fetched {
    Downloaded,
    NotModified,
}

/// A connection slower than this many bytes per second for `SLOW_SECS`
/// is dropped and the download resumes on a fresh connection. Hosts like
/// SpaceDock sometimes hand out one crawling connection while a new one is
/// fast.
const SLOW_BYTES_PER_SEC: &str = "131072";
const SLOW_SECS: &str = "15";
/// Attempts in a row that add no bytes before a download gives up.
const MAX_STALLS: usize = 4;

/// Downloads into `dest`, reporting the bytes written so far.
///
/// With `etag`, this is a conditional fetch of a single file: the saved
/// ETag is sent and `NotModified` comes back when nothing changed.
///
/// Without it, all URLs are treated as mirrors of the same file: a slow or
/// broken connection is dropped and the download resumes from the partial
/// file, moving on to the next URL. The partial file is kept on failure so
/// a later attempt resumes it (the caller verifies hashes afterwards).
pub fn fetch(
    urls: &[String],
    dest: &Path,
    etag: Option<&Path>,
    progress: &mut dyn FnMut(u64),
) -> Result<Fetched> {
    if urls.is_empty() {
        bail!("no download URL");
    }
    let dir = dest.parent().context("destination has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let part = dest.with_extension("part");
    let finish = |fetched: Fetched| -> Result<Fetched> {
        match fetched {
            Fetched::NotModified => {
                let _ = fs::remove_file(&part);
            }
            Fetched::Downloaded => {
                fs::rename(&part, dest)
                    .with_context(|| format!("move download to {}", dest.display()))?;
            }
        }
        Ok(fetched)
    };

    let mut errors = Vec::new();
    if let Some(etag) = etag {
        for url in urls {
            let _ = fs::remove_file(&part);
            match run_curl(url, &part, Some(etag), false, progress)? {
                Ok(f) => return finish(f),
                Err(Failure::Refused(e) | Failure::Transient(e)) => {
                    errors.push(format!("{url}: {e}"))
                }
            }
        }
        let _ = fs::remove_file(&part);
        bail!("download failed:\n  {}", errors.join("\n  "));
    }

    let mut live: Vec<&String> = urls.iter().collect();
    let mut stalls = 0;
    let mut turn = 0;
    while !live.is_empty() {
        let idx = turn % live.len();
        let url = live[idx];
        let before = file_len(&part);
        match run_curl(url, &part, None, before > 0, progress)? {
            Ok(f) => return finish(f),
            Err(Failure::Refused(e)) if before > 0 => {
                // Maybe the server can't resume: start this source over.
                log::info!("{url}: {e}; restarting from scratch");
                let _ = fs::remove_file(&part);
            }
            Err(Failure::Refused(e)) => {
                errors.push(format!("{url}: {e}"));
                live.remove(idx);
            }
            Err(Failure::Transient(e)) => {
                if file_len(&part) > before {
                    stalls = 0;
                } else {
                    stalls += 1;
                    errors.push(format!("{url}: {e}"));
                }
                if stalls >= MAX_STALLS {
                    break;
                }
                log::info!("{url}: {e}; resuming on a new connection");
                turn += 1;
            }
        }
    }
    bail!("download failed:\n  {}", errors.join("\n  "))
}

fn file_len(p: &Path) -> u64 {
    fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Why one curl run failed.
enum Failure {
    /// The server answered with an error (or can't resume): this source
    /// won't work as is.
    Refused(String),
    /// Network trouble or a too-slow connection: worth another try.
    Transient(String),
}

/// Runs curl once. The outer error is for curl not starting at all.
fn run_curl(
    url: &str,
    part: &Path,
    etag: Option<&Path>,
    resume: bool,
    progress: &mut dyn FnMut(u64),
) -> Result<Result<Fetched, Failure>> {
    let mut cmd = Command::new("curl");
    cmd.args(["--fail", "--location", "--silent", "--show-error"])
        .args(["--connect-timeout", "30", "--user-agent", &user_agent()])
        .args([
            "--speed-limit",
            SLOW_BYTES_PER_SEC,
            "--speed-time",
            SLOW_SECS,
        ])
        .args(["--write-out", "%{http_code}", "--output"])
        .arg(part);
    if resume {
        cmd.args(["--continue-at", "-"]);
    }
    if let Some(etag) = etag {
        if etag.is_file() {
            cmd.arg("--etag-compare").arg(etag);
        }
        cmd.arg("--etag-save").arg(etag);
    }
    cmd.arg("--").arg(url);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("run curl (is it installed?)")?;
    loop {
        if let Some(status) = child.try_wait().context("wait for curl")? {
            let mut code = String::new();
            let mut err = String::new();
            if let Some(mut out) = child.stdout.take() {
                out.read_to_string(&mut code).ok();
            }
            if let Some(mut e) = child.stderr.take() {
                e.read_to_string(&mut err).ok();
            }
            let err = err.trim().to_owned();
            return Ok(match status.code() {
                Some(0) => {
                    if code.trim() == "304" {
                        Ok(Fetched::NotModified)
                    } else {
                        if !part.exists() {
                            // curl succeeded but wrote nothing (an empty file).
                            File::create(part)
                                .with_context(|| format!("create {}", part.display()))?;
                        }
                        progress(file_len(part));
                        Ok(Fetched::Downloaded)
                    }
                }
                // 22: HTTP error status. 33: the server can't resume.
                // 36: bad resume offset.
                Some(22 | 33 | 36) => Err(Failure::Refused(err)),
                _ => Err(Failure::Transient(err)),
            });
        }
        progress(file_len(part));
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Checks a file against CKAN's hashes (SHA-256 preferred, then SHA-1).
/// A module without hashes passes.
pub fn verify(path: &Path, hash: Option<&DownloadHash>) -> Result<()> {
    let Some(hash) = hash else { return Ok(()) };
    if let Some(want) = &hash.sha256 {
        let got = file_digest::<Sha256>(path)?;
        if !got.eq_ignore_ascii_case(want) {
            bail!(
                "SHA-256 mismatch for {}: expected {want}, got {got}",
                path.display()
            );
        }
    } else if let Some(want) = &hash.sha1 {
        let got = file_digest::<Sha1>(path)?;
        if !got.eq_ignore_ascii_case(want) {
            bail!(
                "SHA-1 mismatch for {}: expected {want}, got {got}",
                path.display()
            );
        }
    }
    Ok(())
}

fn file_digest<D: Digest>(path: &Path) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = D::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Lowercase hex of a byte slice.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Where a module's archive is cached. Named after its download URL, so
/// modules that share one archive (Scatterer and its configs) share the file.
pub fn cache_path(dir: &Path, m: &Module) -> PathBuf {
    let url = m.download.first().map_or("", String::as_str);
    let key = hex(&Sha1::digest(url.as_bytes()));
    dir.join(format!(
        "{}-{}-{}.zip",
        &key[..10],
        slug(&m.identifier),
        slug(m.version.as_str())
    ))
}

/// Whether a module's archive is already cached and valid.
pub fn is_cached(dir: &Path, m: &Module) -> bool {
    let p = cache_path(dir, m);
    p.is_file()
        && m.download_size
            .is_none_or(|s| fs::metadata(&p).map(|md| md.len()).ok() == Some(s))
}

/// Progress from [`download_modules`].
#[derive(Debug)]
pub enum DownloadEvent {
    Progress {
        identifier: String,
        bytes: u64,
        total: Option<u64>,
    },
    Done {
        identifier: String,
    },
}

/// Downloads every module that isn't cached, `workers` at a time, and
/// verifies each against its hashes. Returns the cached archive paths in
/// the same order as `mods`.
pub fn download_modules(
    dir: &Path,
    all: &[&Module],
    workers: usize,
    events: &mpsc::Sender<DownloadEvent>,
) -> Result<Vec<PathBuf>> {
    // Fetch each archive once, even when several modules share it.
    let mut unique: Vec<&Module> = Vec::new();
    for m in all {
        if !unique
            .iter()
            .any(|u| u.download.first() == m.download.first())
        {
            unique.push(m);
        }
    }
    let mods = unique.as_slice();
    let next = AtomicUsize::new(0);
    let (err_tx, err_rx) = mpsc::channel::<String>();
    std::thread::scope(|s| {
        for _ in 0..workers.clamp(1, 16) {
            let err_tx = err_tx.clone();
            let next = &next;
            s.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(m) = mods.get(i) else { break };
                    if let Err(e) = download_one(dir, m, events) {
                        let _ = err_tx.send(format!("{} {}: {e:#}", m.identifier, m.version));
                    }
                }
            });
        }
    });
    drop(err_tx);
    let errors: Vec<String> = err_rx.iter().collect();
    if !errors.is_empty() {
        bail!("{}", errors.join("\n"));
    }
    Ok(all
        .iter()
        .map(|m| {
            let owner = mods
                .iter()
                .find(|u| u.download.first() == m.download.first())
                .expect("every module has a unique representative");
            cache_path(dir, owner)
        })
        .collect())
}

fn download_one(dir: &Path, m: &Module, events: &mpsc::Sender<DownloadEvent>) -> Result<()> {
    let dest = cache_path(dir, m);
    if dest.is_file() && verify(&dest, m.download_hash.as_ref()).is_ok() {
        let _ = events.send(DownloadEvent::Done {
            identifier: m.identifier.clone(),
        });
        return Ok(());
    }
    let id = m.identifier.clone();
    let total = m.download_size;
    let mut report = |bytes| {
        let _ = events.send(DownloadEvent::Progress {
            identifier: id.clone(),
            bytes,
            total,
        });
    };
    let mut urls = m.download.clone();
    if let Some(mirror) = m.mirror_url()
        && !urls.contains(&mirror)
    {
        urls.push(mirror);
    }
    fetch(&urls, &dest, None, &mut report)?;
    if let Err(e) = verify(&dest, m.download_hash.as_ref()) {
        let _ = fs::remove_file(&dest);
        return Err(e);
    }
    let _ = events.send(DownloadEvent::Done {
        identifier: m.identifier.clone(),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_hashes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("f");
        fs::write(&p, b"abc").expect("write");
        let good = DownloadHash {
            sha1: Some("A9993E364706816ABA3E25717850C26C9CD0D89D".into()),
            sha256: None,
        };
        verify(&p, Some(&good)).expect("sha1 matches");
        let good256 = DownloadHash {
            sha1: None,
            sha256: Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into()),
        };
        verify(&p, Some(&good256)).expect("sha256 matches");
        let bad = DownloadHash {
            sha1: Some("00".into()),
            sha256: None,
        };
        assert!(verify(&p, Some(&bad)).is_err());
    }

    #[test]
    fn resumes_a_partial_download() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src.bin");
        let body: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&src, &body).expect("write");
        let dest = tmp.path().join("dest.bin");
        fs::write(dest.with_extension("part"), &body[..40_000]).expect("write part");
        let urls = vec![
            format!("file://{}", tmp.path().join("missing.bin").display()),
            format!("file://{}", src.display()),
        ];
        fetch(&urls, &dest, None, &mut |_| {}).expect("fetch");
        assert_eq!(fs::read(&dest).expect("read"), body);
        assert!(!dest.with_extension("part").exists());
    }

    #[test]
    fn fetches_file_urls() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src.bin");
        fs::write(&src, b"payload").expect("write");
        let dest = tmp.path().join("out/dest.bin");
        let url = format!("file://{}", src.display());
        let mut last = 0;
        let got = fetch(&[url], &dest, None, &mut |b| last = b).expect("fetch");
        assert_eq!(got, Fetched::Downloaded);
        assert_eq!(fs::read(&dest).expect("read"), b"payload");
        assert_eq!(last, 7);
    }
}
