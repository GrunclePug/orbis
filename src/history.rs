//! Change history: every applied change is recorded with the instance's
//! mod list before and after, so any earlier state can be restored.
//! Stored in `<instance>/history.json`.

use std::collections::BTreeMap;
use std::fs;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::instance::{Instance, ModSource, Registry};
use crate::manual::{self, ManualMod};
use crate::paths::{self, Paths};
use crate::repo::Index;
use crate::resolver::Request;
use crate::version::ModVersion;

/// Entries kept per instance.
const KEEP: usize = 200;

/// One installed mod in a snapshot.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapMod {
    pub identifier: String,
    pub version: ModVersion,
    pub source: ModSource,
    pub auto: bool,
}

/// One applied change.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// Milliseconds since the epoch; unique per instance.
    pub id: u64,
    pub time: u64,
    /// One line per change ("install X 1.0 (requested)").
    pub summary: Vec<String>,
    pub before: Vec<SnapMod>,
    pub after: Vec<SnapMod>,
}

/// The mod list of a registry.
pub fn snapshot(reg: &Registry) -> Vec<SnapMod> {
    reg.mods
        .iter()
        .map(|(id, m)| SnapMod {
            identifier: id.clone(),
            version: m.module.version.clone(),
            source: m.source,
            auto: m.auto_installed,
        })
        .collect()
}

/// Every entry, newest first.
pub fn list(inst: &Instance) -> Result<Vec<Entry>> {
    let path = inst.dir.join("history.json");
    let mut entries: Vec<Entry> = match fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    entries.sort_by_key(|e| std::cmp::Reverse(e.id));
    Ok(entries)
}

/// Records a change. Does nothing when the mod list didn't change.
pub fn record(
    inst: &Instance,
    before: Vec<SnapMod>,
    after: Vec<SnapMod>,
    summary: Vec<String>,
) -> Result<()> {
    if before == after {
        return Ok(());
    }
    let mut entries = list(inst)?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let id = entries.first().map_or(millis, |e| millis.max(e.id + 1));
    entries.insert(
        0,
        Entry {
            id,
            time: paths::now_secs(),
            summary,
            before,
            after,
        },
    );
    entries.truncate(KEEP);
    let bytes = serde_json::to_vec_pretty(&entries).context("serialize history")?;
    paths::write_atomic(&inst.dir.join("history.json"), &bytes)
}

/// What restoring a snapshot takes.
#[derive(Debug, Default)]
pub struct RestorePlan {
    /// CKAN removals and installs (pinned versions).
    pub request: Request,
    /// Manual mods to install from the library afterwards.
    pub manual: Vec<ManualMod>,
    pub warnings: Vec<String>,
}

impl RestorePlan {
    pub fn is_empty(&self) -> bool {
        self.request.install.is_empty() && self.request.remove.is_empty() && self.manual.is_empty()
    }
}

/// Works out how to get from the instance's current mods to `target`.
pub fn restore_plan(
    paths: &Paths,
    inst: &Instance,
    index: &Index,
    target: &[SnapMod],
) -> Result<RestorePlan> {
    let reg = inst.registry()?;
    let current: BTreeMap<&str, &ModVersion> = reg
        .mods
        .iter()
        .map(|(id, m)| (id.as_str(), &m.module.version))
        .collect();
    let wanted: BTreeMap<&str, &SnapMod> =
        target.iter().map(|m| (m.identifier.as_str(), m)).collect();
    let mut plan = RestorePlan::default();
    for id in current.keys() {
        if !wanted.contains_key(id) {
            plan.request.remove.push((*id).to_owned());
        }
    }
    let library = manual::list(paths)?;
    let compat = inst.compat_versions();
    for m in target {
        let same = current
            .get(m.identifier.as_str())
            .is_some_and(|v| v.as_str() == m.version.as_str());
        if same {
            continue;
        }
        match m.source {
            ModSource::Ckan => {
                if index.get(&m.identifier, &m.version).is_some() {
                    plan.request
                        .install
                        .push((m.identifier.clone(), Some(m.version.clone())));
                } else if index
                    .latest_compatible(&m.identifier, &compat, inst.config.allow_testing)
                    .is_some()
                {
                    plan.warnings.push(format!(
                        "{} {} is gone from CKAN; using the newest compatible release",
                        m.identifier, m.version
                    ));
                    plan.request.install.push((m.identifier.clone(), None));
                } else {
                    plan.warnings.push(format!(
                        "{} {} can't be installed any more; skipped",
                        m.identifier, m.version
                    ));
                }
            }
            ModSource::Manual => {
                let key = m
                    .identifier
                    .strip_prefix(manual::PREFIX)
                    .unwrap_or(&m.identifier);
                match library
                    .iter()
                    .find(|l| l.id == key && l.version == m.version.as_str())
                {
                    Some(l) => {
                        if current.contains_key(m.identifier.as_str()) {
                            plan.request.remove.push(m.identifier.clone());
                        }
                        plan.manual.push(l.clone());
                    }
                    None => plan.warnings.push(format!(
                        "manual mod {} {} is no longer in the library; skipped",
                        key, m.version
                    )),
                }
            }
        }
    }
    Ok(plan)
}

/// After a restore, puts the dependency flags back as they were.
pub fn restore_flags(inst: &Instance, target: &[SnapMod]) -> Result<()> {
    let mut reg = inst.registry()?;
    for t in target {
        if let Some(m) = reg.mods.get_mut(&t.identifier) {
            m.auto_installed = t.auto;
        }
    }
    inst.save_registry(&reg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: &str, v: &str, auto: bool) -> SnapMod {
        SnapMod {
            identifier: id.into(),
            version: ModVersion::new(v),
            source: ModSource::Ckan,
            auto,
        }
    }

    #[test]
    fn records_newest_first_and_skips_no_ops() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inst = Instance {
            id: "t".into(),
            dir: tmp.path().to_path_buf(),
            config: toml::from_str("name = \"t\"\ngame_version = \"1.12.5\"\ncreated = 0\n")
                .expect("config"),
        };
        record(
            &inst,
            vec![],
            vec![snap("A", "1", false)],
            vec!["install A 1".into()],
        )
        .expect("record");
        record(
            &inst,
            vec![snap("A", "1", false)],
            vec![snap("A", "1", false)],
            vec![],
        )
        .expect("no-op");
        record(
            &inst,
            vec![snap("A", "1", false)],
            vec![snap("A", "2", false)],
            vec!["upgrade A".into()],
        )
        .expect("record");
        let all = list(&inst).expect("list");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].summary, ["upgrade A"]);
        assert!(all[0].id > all[1].id);
    }
}
