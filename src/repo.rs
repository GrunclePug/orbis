//! The CKAN module index: fetched as CKAN-meta tarballs, cached as JSON.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::download::{self, Fetched};
use crate::metadata::{Kind, Module, ReleaseStatus};
use crate::paths::{self, Paths, slug};
use crate::version::{GameVersion, ModVersion};

/// Every known module, grouped by identifier, newest release first.
#[derive(Debug, Default)]
pub struct Index {
    modules: BTreeMap<String, Vec<Module>>,
    /// Virtual name -> identifiers of modules that provide it.
    provides: HashMap<String, Vec<String>>,
    /// Download counts from CKAN's `download_counts.json`.
    pub downloads: HashMap<String, u64>,
    /// When the index was built (Unix seconds).
    pub updated: u64,
}

#[derive(Serialize, Deserialize)]
struct IndexFile {
    updated: u64,
    downloads: HashMap<String, u64>,
    modules: Vec<Module>,
}

/// What an update did.
#[derive(Debug)]
pub struct UpdateSummary {
    pub modules: usize,
    pub releases: usize,
    pub skipped: usize,
    pub unchanged: bool,
}

impl Index {
    /// Builds an index from a list of releases. When the same identifier and
    /// version appear twice, the first one wins.
    pub fn from_modules(list: Vec<Module>, downloads: HashMap<String, u64>, updated: u64) -> Self {
        let mut modules: BTreeMap<String, Vec<Module>> = BTreeMap::new();
        for m in list {
            let versions = modules.entry(m.identifier.clone()).or_default();
            if !versions
                .iter()
                .any(|o| o.version.as_str() == m.version.as_str())
            {
                versions.push(m);
            }
        }
        let mut provides: HashMap<String, Vec<String>> = HashMap::new();
        for (id, versions) in &mut modules {
            versions.sort_by(|a, b| b.version.cmp(&a.version));
            let mut names: Vec<&String> = versions.iter().flat_map(|m| &m.provides).collect();
            names.sort();
            names.dedup();
            for name in names {
                provides.entry(name.clone()).or_default().push(id.clone());
            }
        }
        Self {
            modules,
            provides,
            downloads,
            updated,
        }
    }

    /// Loads the cached index, or `None` if there isn't one yet.
    pub fn load(paths: &Paths) -> Result<Option<Self>> {
        let path = paths.repo().join("index.json");
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
        };
        let data: IndexFile = serde_json::from_reader(std::io::BufReader::new(file))
            .with_context(|| format!("parse {}", path.display()))?;
        Ok(Some(Self::from_modules(
            data.modules,
            data.downloads,
            data.updated,
        )))
    }

    fn save(&self, paths: &Paths) -> Result<()> {
        let data = IndexFile {
            updated: self.updated,
            downloads: self.downloads.clone(),
            modules: self.modules.values().flatten().cloned().collect(),
        };
        let bytes = serde_json::to_vec(&data).context("serialize index")?;
        paths::write_atomic(&paths.repo().join("index.json"), &bytes)
    }

    /// Number of distinct identifiers.
    pub fn len(&self) -> usize {
        self.modules.len()
    }

    /// Whether the index has no modules.
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    /// All identifiers with their releases, newest first.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Vec<Module>)> {
        self.modules.iter()
    }

    /// Every release of one identifier, newest first.
    pub fn versions(&self, identifier: &str) -> &[Module] {
        self.modules.get(identifier).map_or(&[], Vec::as_slice)
    }

    /// One exact release.
    pub fn get(&self, identifier: &str, version: &ModVersion) -> Option<&Module> {
        self.versions(identifier)
            .iter()
            .find(|m| m.version.as_str() == version.as_str())
            .or_else(|| {
                self.versions(identifier)
                    .iter()
                    .find(|m| &m.version == version)
            })
    }

    /// The newest release of an identifier that suits the game versions.
    pub fn latest_compatible(
        &self,
        identifier: &str,
        compat: &[GameVersion],
        allow_testing: bool,
    ) -> Option<&Module> {
        self.versions(identifier)
            .iter()
            .find(|m| installable(m, compat, allow_testing))
    }

    /// Identifiers of modules that provide `name` (including a module
    /// actually named `name`), exact identifier first.
    pub fn providers(&self, name: &str) -> Vec<&str> {
        let mut out = Vec::new();
        if let Some((id, _)) = self.modules.get_key_value(name) {
            out.push(id.as_str());
        }
        if let Some(ids) = self.provides.get(name) {
            out.extend(ids.iter().map(String::as_str).filter(|id| *id != name));
        }
        out
    }

    /// Download count for an identifier.
    pub fn download_count(&self, identifier: &str) -> u64 {
        self.downloads.get(identifier).copied().unwrap_or(0)
    }
}

/// Whether a release can be offered for install on these game versions.
pub fn installable(m: &Module, compat: &[GameVersion], allow_testing: bool) -> bool {
    m.kind != Kind::Dlc
        && m.compatible_with(compat)
        && (m.release_status == ReleaseStatus::Stable
            || allow_testing && m.release_status == ReleaseStatus::Testing)
}

/// Fetches every configured repository and rebuilds the cached index.
/// `progress` receives status lines.
pub fn update(
    paths: &Paths,
    config: &Config,
    progress: &mut dyn FnMut(String),
) -> Result<UpdateSummary> {
    let dir = paths.repo();
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let have_index = dir.join("index.json").is_file();
    let mut changed = false;
    let mut archives = Vec::new();
    for repo in &config.repositories {
        let name = slug(&repo.name);
        let archive = dir.join(format!("{name}.tar.gz"));
        let etag = dir.join(format!("{name}.etag"));
        progress(format!("Fetching {}", repo.name));
        if !archive.is_file() || !have_index {
            // Without the archive (or an index built from it), a 304 would
            // leave nothing to read.
            let _ = fs::remove_file(&etag);
        }
        let mut report = |bytes: u64| {
            progress(format!(
                "Fetching {}: {:.1} MiB",
                repo.name,
                bytes as f64 / 1048576.0
            ))
        };
        match download::fetch(
            std::slice::from_ref(&repo.url),
            &archive,
            Some(&etag),
            &mut report,
        )
        .with_context(|| format!("fetch repository {}", repo.name))?
        {
            Fetched::Downloaded => changed = true,
            Fetched::NotModified => {}
        }
        archives.push(archive);
    }
    if !changed && have_index {
        let index = Index::load(paths)?.unwrap_or_default();
        return Ok(UpdateSummary {
            modules: index.len(),
            releases: index.iter().map(|(_, v)| v.len()).sum(),
            skipped: 0,
            unchanged: true,
        });
    }
    let mut all = Vec::new();
    let mut downloads = HashMap::new();
    let mut skipped = 0;
    for archive in &archives {
        progress(format!("Reading {}", archive.display()));
        let parsed = parse_tarball(archive)?;
        skipped += parsed.skipped;
        all.extend(parsed.modules);
        for (k, v) in parsed.downloads {
            downloads.entry(k).or_insert(v);
        }
    }
    let releases = all.len();
    let index = Index::from_modules(all, downloads, paths::now_secs());
    index.save(paths)?;
    Ok(UpdateSummary {
        modules: index.len(),
        releases,
        skipped,
        unchanged: false,
    })
}

/// What a CKAN-meta tarball holds.
pub struct Parsed {
    pub modules: Vec<Module>,
    pub downloads: HashMap<String, u64>,
    /// `.ckan` files that failed to parse.
    pub skipped: usize,
}

/// Reads every `.ckan` file and `download_counts.json` from a tarball.
pub fn parse_tarball(path: &Path) -> Result<Parsed> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut archive = tar::Archive::new(GzDecoder::new(std::io::BufReader::new(file)));
    let mut out = Parsed {
        modules: Vec::new(),
        downloads: HashMap::new(),
        skipped: 0,
    };
    let mut buf = Vec::new();
    for entry in archive.entries().context("read tarball")? {
        let mut entry = entry.context("read tarball entry")?;
        let name = entry
            .path()
            .context("entry path")?
            .to_string_lossy()
            .into_owned();
        let is_ckan = name.ends_with(".ckan");
        let is_counts = name.ends_with("/download_counts.json") || name == "download_counts.json";
        if !is_ckan && !is_counts {
            continue;
        }
        buf.clear();
        entry
            .read_to_end(&mut buf)
            .with_context(|| format!("read {name}"))?;
        if is_counts {
            match serde_json::from_slice(&buf) {
                Ok(counts) => out.downloads = counts,
                Err(e) => log::warn!("{name}: {e}"),
            }
            continue;
        }
        match serde_json::from_slice::<Module>(&buf) {
            Ok(m) => out.modules.push(m),
            Err(e) => {
                log::debug!("skip {name}: {e}");
                out.skipped += 1;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A minimal module for tests.
    pub fn module(id: &str, version: &str) -> Module {
        serde_json::from_value(serde_json::json!({
            "identifier": id,
            "version": version,
            "ksp_version": "1.12",
            "download": format!("https://example.invalid/{id}.zip"),
        }))
        .expect("test module parses")
    }

    #[test]
    fn indexes_versions_and_provides() {
        let mut a = module("A", "1.0");
        a.provides = vec!["Virt".into()];
        let a2 = module("A", "1.10");
        let mut b = module("B", "2");
        b.provides = vec!["Virt".into()];
        let mut old = module("Old", "1");
        old.ksp_version = GameVersion::parse("1.7");
        let index = Index::from_modules(vec![a, a2, b, old], HashMap::new(), 0);
        assert_eq!(index.versions("A")[0].version.as_str(), "1.10");
        assert_eq!(index.providers("Virt"), vec!["A", "B"]);
        let compat = [GameVersion::parse("1.12.5").expect("valid")];
        assert!(index.latest_compatible("Old", &compat, false).is_none());
        assert_eq!(
            index
                .latest_compatible("A", &compat, false)
                .map(|m| m.version.as_str()),
            Some("1.10")
        );
    }

    #[test]
    fn parses_a_tarball() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("meta.tar.gz");
        let gz = flate2::write::GzEncoder::new(
            File::create(&path).expect("create"),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(gz);
        let mut add = |name: &str, body: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, name, body).expect("append");
        };
        add(
            "CKAN-meta-master/A/A-1.0.ckan",
            br#"{"identifier":"A","version":"1.0"}"#,
        );
        add("CKAN-meta-master/B/B-1.ckan", b"not json");
        add("CKAN-meta-master/download_counts.json", br#"{"A": 42}"#);
        tar.into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gz");

        let parsed = parse_tarball(&path).expect("parse");
        assert_eq!(parsed.modules.len(), 1);
        assert_eq!(parsed.skipped, 1);
        assert_eq!(parsed.downloads.get("A"), Some(&42));
    }
}
