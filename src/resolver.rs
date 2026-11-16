//! Working out a consistent set of changes: what to install for
//! dependencies, what removing a mod takes with it, and what conflicts.
//!
//! The approach matches CKAN's resolver: pick the newest compatible
//! release of each requested mod, then satisfy dependencies greedily.
//! When several mods could satisfy one dependency, the caller must
//! choose (CKAN asks the user too).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt;

use crate::metadata::{Direct, Kind, Module, Relationship};
use crate::repo::{self, Index};
use crate::version::{GameVersion, ModVersion};

/// Something already in the instance.
#[derive(Clone, Debug)]
pub struct Present {
    pub module: Module,
    pub auto_installed: bool,
    /// DLC: detected, never installed or removed by orbis.
    pub is_dlc: bool,
    /// Version unknown (DLC without a version line): satisfies any bound.
    pub unknown_version: bool,
}

impl Present {
    /// A detected DLC.
    pub fn dlc(identifier: &str, version: Option<ModVersion>) -> Self {
        let unknown = version.is_none();
        let module = Module {
            identifier: identifier.to_owned(),
            name: identifier.to_owned(),
            summary: String::new(),
            description: None,
            version: version.unwrap_or_else(|| ModVersion::new("0")),
            author: vec![],
            license: vec![],
            kind: Kind::Dlc,
            release_status: Default::default(),
            ksp_version: None,
            ksp_version_min: None,
            ksp_version_max: None,
            depends: vec![],
            recommends: vec![],
            suggests: vec![],
            supports: vec![],
            conflicts: vec![],
            provides: vec![],
            download: vec![],
            download_size: None,
            download_hash: None,
            install_size: None,
            install: None,
            resources: Default::default(),
            tags: vec![],
            release_date: None,
            replaced_by: None,
        };
        Self {
            module,
            auto_installed: false,
            is_dlc: true,
            unknown_version: unknown,
        }
    }
}

/// What the user asked for.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// Identifiers (or virtual names) to install, optionally pinned.
    pub install: Vec<(String, Option<ModVersion>)>,
    /// Identifiers to remove.
    pub remove: Vec<String>,
    /// Identifiers to upgrade to their newest compatible release.
    pub upgrade: Vec<String>,
    /// Answers to earlier [`ResolveError::Choice`] questions: relationship
    /// text (as shown in the error) or virtual name -> chosen identifier.
    pub choices: HashMap<String, String>,
}

/// Why a change is in the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    Requested,
    /// Needed by this identifier.
    Dependency(String),
    /// Depends on this identifier, which is being removed or changed.
    BrokenBy(String),
    /// Was installed as a dependency and nothing needs it any more.
    NoLongerNeeded,
    /// A manual mod that provides it takes its place.
    ReplacedBy(String),
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Requested => f.write_str("requested"),
            Reason::Dependency(by) => write!(f, "needed by {by}"),
            Reason::BrokenBy(by) => write!(f, "depends on {by}"),
            Reason::NoLongerNeeded => f.write_str("no longer needed"),
            Reason::ReplacedBy(by) => write!(f, "replaced by {by}"),
        }
    }
}

/// One step of a plan.
#[derive(Clone, Debug)]
pub enum Change {
    Install {
        module: Module,
        reason: Reason,
    },
    Upgrade {
        from: ModVersion,
        module: Module,
        reason: Reason,
    },
    Remove {
        identifier: String,
        version: ModVersion,
        reason: Reason,
    },
}

impl Change {
    pub fn identifier(&self) -> &str {
        match self {
            Change::Install { module, .. } | Change::Upgrade { module, .. } => &module.identifier,
            Change::Remove { identifier, .. } => identifier,
        }
    }
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::Install { module, reason } => {
                write!(
                    f,
                    "install {} {} ({reason})",
                    module.identifier, module.version
                )
            }
            Change::Upgrade {
                from,
                module,
                reason,
            } => {
                write!(
                    f,
                    "upgrade {} {from} -> {} ({reason})",
                    module.identifier, module.version
                )
            }
            Change::Remove {
                identifier,
                version,
                reason,
            } => write!(f, "remove {identifier} {version} ({reason})"),
        }
    }
}

/// A recommended or suggested mod the plan doesn't include.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    /// The mod making the recommendation.
    pub by: String,
    pub relationship: String,
    /// Installable identifiers that satisfy it.
    pub candidates: Vec<String>,
    /// `true` for recommends, `false` for suggests.
    pub recommended: bool,
}

/// A consistent set of changes.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub changes: Vec<Change>,
    pub suggestions: Vec<Suggestion>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// Why no plan could be made.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("no mod named {0:?}")]
    NotFound(String),
    #[error("{identifier} has no release compatible with this instance's game versions")]
    NoCompatible { identifier: String },
    #[error("{identifier} {version} not found")]
    NoSuchVersion { identifier: String, version: String },
    #[error("{0} is not installed")]
    NotInstalled(String),
    #[error("{0} is a DLC; orbis detects it but can't remove it")]
    IsDlc(String),
    #[error("{by} needs {relationship}, which can be satisfied by several mods: {}", candidates.join(", "))]
    Choice {
        by: String,
        relationship: String,
        candidates: Vec<String>,
    },
    #[error("{by} needs {relationship}, but no compatible mod provides it")]
    Unsatisfiable { by: String, relationship: String },
    #[error("{a} conflicts with {b}")]
    Conflict { a: String, b: String },
}

#[derive(Clone, Debug)]
struct Entry {
    module: Module,
    unknown_version: bool,
    auto: bool,
    is_dlc: bool,
    /// `None` for mods already present and untouched.
    reason: Option<Reason>,
    upgraded_from: Option<ModVersion>,
}

impl Entry {
    fn version(&self) -> Option<&ModVersion> {
        if self.unknown_version {
            None
        } else {
            Some(&self.module.version)
        }
    }

    fn satisfies(&self, rel: &Relationship) -> bool {
        rel.matches(
            &self.module.identifier,
            self.version(),
            &self.module.provides,
        )
    }
}

/// Context for resolving: the index, the game versions, and what's present.
pub struct Resolver<'a> {
    pub index: &'a Index,
    pub compat: &'a [GameVersion],
    pub allow_testing: bool,
}

impl Resolver<'_> {
    /// Produces a plan for `req` against the `present` mods.
    pub fn resolve(&self, present: &[Present], req: &Request) -> Result<Plan, ResolveError> {
        let mut state: BTreeMap<String, Entry> = present
            .iter()
            .map(|p| {
                (
                    p.module.identifier.clone(),
                    Entry {
                        module: p.module.clone(),
                        unknown_version: p.unknown_version,
                        auto: p.auto_installed,
                        is_dlc: p.is_dlc,
                        reason: None,
                        upgraded_from: None,
                    },
                )
            })
            .collect();
        let broken_before: BTreeSet<String> = state
            .keys()
            .filter(|id| !deps_satisfied(&state, id))
            .cloned()
            .collect();
        let mut removed: Vec<Change> = Vec::new();
        let mut removed_ids: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();

        for id in &req.remove {
            let e = state
                .get(id)
                .ok_or_else(|| ResolveError::NotInstalled(id.clone()))?;
            if e.is_dlc {
                return Err(ResolveError::IsDlc(id.clone()));
            }
            let e = state.remove(id).expect("checked present above");
            removed_ids.insert(id.clone());
            removed.push(Change::Remove {
                identifier: id.clone(),
                version: e.module.version,
                reason: Reason::Requested,
            });
        }

        for id in &req.upgrade {
            let e = state
                .get(id)
                .ok_or_else(|| ResolveError::NotInstalled(id.clone()))?;
            if e.is_dlc || id.starts_with("manual:") {
                continue;
            }
            if let Some(m) = self
                .index
                .latest_compatible(id, self.compat, self.allow_testing)
                && m.version > e.module.version
            {
                self.place(&mut state, m.clone(), Reason::Requested, false, &mut queue);
            }
        }

        for (id, version) in &req.install {
            let m =
                match version {
                    Some(v) => self.index.get(id, v).cloned().ok_or_else(|| {
                        ResolveError::NoSuchVersion {
                            identifier: id.clone(),
                            version: v.to_string(),
                        }
                    })?,
                    None => match self
                        .index
                        .latest_compatible(id, self.compat, self.allow_testing)
                    {
                        Some(m) => m.clone(),
                        None if !self.index.versions(id).is_empty() => {
                            return Err(ResolveError::NoCompatible {
                                identifier: id.clone(),
                            });
                        }
                        None => {
                            // Maybe a virtual name: resolve it like a dependency.
                            let rel = Relationship::Direct(Direct::named(id.clone()));
                            if state.values().any(|e| e.satisfies(&rel)) {
                                continue;
                            }
                            if self.index.providers(id).is_empty() {
                                return Err(ResolveError::NotFound(id.clone()));
                            }
                            let m = self.pick(&state, &removed_ids, &rel, "you", &req.choices)?;
                            self.place(&mut state, m, Reason::Requested, false, &mut queue);
                            continue;
                        }
                    },
                };
            if let Some(e) = state.get(id)
                && e.module.version.as_str() == m.version.as_str()
            {
                continue;
            }
            self.place(&mut state, m, Reason::Requested, false, &mut queue);
        }

        while let Some(id) = queue.pop_front() {
            let Some(entry) = state.get(&id) else {
                continue;
            };
            let depends = entry.module.depends.clone();
            for rel in &depends {
                if state.values().any(|e| e.satisfies(rel)) {
                    continue;
                }
                let m = self.pick(&state, &removed_ids, rel, &id, &req.choices)?;
                self.place(
                    &mut state,
                    m,
                    Reason::Dependency(id.clone()),
                    true,
                    &mut queue,
                );
            }
        }

        // Present mods whose dependencies broke go too, then any automatic
        // dependency nothing needs any more. Repeat until nothing changes.
        loop {
            let mut changed = false;
            let ids: Vec<String> = state.keys().cloned().collect();
            for id in ids {
                let e = &state[&id];
                if e.reason.is_some() || e.is_dlc || broken_before.contains(&id) {
                    continue;
                }
                if !deps_satisfied(&state, &id) {
                    let missing = first_missing(&state, &id).unwrap_or_default();
                    let e = state.remove(&id).expect("id came from state");
                    removed_ids.insert(id.clone());
                    removed.push(Change::Remove {
                        identifier: id,
                        version: e.module.version,
                        reason: Reason::BrokenBy(missing),
                    });
                    changed = true;
                }
            }
            if !removed.is_empty() || state.values().any(|e| e.upgraded_from.is_some()) {
                let ids: Vec<String> = state.keys().cloned().collect();
                for id in ids {
                    let e = &state[&id];
                    if !e.auto || e.reason.is_some() || e.is_dlc {
                        continue;
                    }
                    let needed = state
                        .iter()
                        .filter(|(other, _)| **other != id)
                        .any(|(_, o)| o.module.depends.iter().any(|r| e.satisfies(r)));
                    if !needed {
                        let e = state.remove(&id).expect("id came from state");
                        removed_ids.insert(id.clone());
                        removed.push(Change::Remove {
                            identifier: id,
                            version: e.module.version,
                            reason: Reason::NoLongerNeeded,
                        });
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }

        // Conflicts involving anything new.
        for (id, e) in &state {
            for (oid, o) in &state {
                if id == oid || (e.reason.is_none() && o.reason.is_none()) {
                    continue;
                }
                // A manual mod replaces the CKAN mods it provides.
                let replaces = e.module.identifier.starts_with(crate::manual::PREFIX)
                    && e.module.provides.contains(&o.module.identifier);
                if replaces || e.module.conflicts.iter().any(|r| o.satisfies(r)) {
                    return Err(ResolveError::Conflict {
                        a: id.clone(),
                        b: oid.clone(),
                    });
                }
            }
        }

        let mut plan = Plan {
            changes: removed,
            suggestions: Vec::new(),
        };
        for (_, e) in state.iter().filter(|(_, e)| e.reason.is_some()) {
            let reason = e.reason.clone().expect("filtered on reason");
            plan.changes.push(match &e.upgraded_from {
                Some(from) => Change::Upgrade {
                    from: from.clone(),
                    module: e.module.clone(),
                    reason,
                },
                None => Change::Install {
                    module: e.module.clone(),
                    reason,
                },
            });
        }
        plan.suggestions = self.suggestions(&state, &removed_ids);
        Ok(plan)
    }

    /// Adds or replaces a module in the state and queues its dependencies.
    fn place(
        &self,
        state: &mut BTreeMap<String, Entry>,
        m: Module,
        reason: Reason,
        auto: bool,
        queue: &mut VecDeque<String>,
    ) {
        let id = m.identifier.clone();
        let previous = state.remove(&id);
        let upgraded_from = previous.as_ref().map(|p| p.module.version.clone());
        let auto = previous.as_ref().map_or(auto, |p| p.auto);
        state.insert(
            id.clone(),
            Entry {
                module: m,
                unknown_version: false,
                auto,
                is_dlc: false,
                reason: Some(reason),
                upgraded_from,
            },
        );
        queue.push_back(id);
    }

    /// Installable candidates for a relationship, newest compatible release
    /// of each identifier, excluding removed and conflicting mods.
    fn candidates(
        &self,
        state: &BTreeMap<String, Entry>,
        removed: &BTreeSet<String>,
        rel: &Relationship,
    ) -> Vec<Module> {
        let mut out: Vec<Module> = Vec::new();
        for d in rel.alternatives() {
            for id in self.index.providers(&d.name) {
                if removed.contains(id) || out.iter().any(|m| m.identifier == id) {
                    continue;
                }
                let found = self.index.versions(id).iter().find(|m| {
                    repo::installable(m, self.compat, self.allow_testing)
                        && d.matches(&m.identifier, Some(&m.version), &m.provides)
                });
                let Some(m) = found else { continue };
                let conflicts = state.values().any(|e| {
                    e.module
                        .conflicts
                        .iter()
                        .any(|r| r.matches(&m.identifier, Some(&m.version), &m.provides))
                        || m.conflicts.iter().any(|r| e.satisfies(r))
                });
                if !conflicts {
                    out.push(m.clone());
                }
            }
        }
        // Exact names first, then the most downloaded.
        let names: Vec<&str> = rel.alternatives().iter().map(|d| d.name.as_str()).collect();
        out.sort_by_key(|m| {
            (
                !names.contains(&m.identifier.as_str()),
                std::cmp::Reverse(self.index.download_count(&m.identifier)),
            )
        });
        out
    }

    fn pick(
        &self,
        state: &BTreeMap<String, Entry>,
        removed: &BTreeSet<String>,
        rel: &Relationship,
        by: &str,
        choices: &HashMap<String, String>,
    ) -> Result<Module, ResolveError> {
        let mut cands = self.candidates(state, removed, rel);
        let text = rel.to_string();
        if cands.is_empty() {
            return Err(ResolveError::Unsatisfiable {
                by: by.to_owned(),
                relationship: text,
            });
        }
        if cands.len() == 1 {
            return Ok(cands.remove(0));
        }
        let names: Vec<&str> = rel.alternatives().iter().map(|d| d.name.as_str()).collect();
        let chosen = choices
            .get(&text)
            .or_else(|| names.iter().find_map(|n| choices.get(*n)));
        if let Some(c) = chosen
            && let Some(i) = cands.iter().position(|m| &m.identifier == c)
        {
            return Ok(cands.remove(i));
        }
        // A single plain relationship naming a real mod picks that mod.
        if let [only] = names.as_slice()
            && let Some(i) = cands.iter().position(|m| m.identifier == *only)
        {
            return Ok(cands.remove(i));
        }
        Err(ResolveError::Choice {
            by: by.to_owned(),
            relationship: text,
            candidates: cands.into_iter().map(|m| m.identifier).collect(),
        })
    }

    fn suggestions(
        &self,
        state: &BTreeMap<String, Entry>,
        removed: &BTreeSet<String>,
    ) -> Vec<Suggestion> {
        let mut out: Vec<Suggestion> = Vec::new();
        for (id, e) in state.iter().filter(|(_, e)| e.reason.is_some()) {
            for (rels, recommended) in [(&e.module.recommends, true), (&e.module.suggests, false)] {
                for rel in rels {
                    if state.values().any(|o| o.satisfies(rel)) {
                        continue;
                    }
                    let relationship = rel.to_string();
                    if out.iter().any(|s| s.relationship == relationship) {
                        continue;
                    }
                    let candidates: Vec<String> = self
                        .candidates(state, removed, rel)
                        .into_iter()
                        .map(|m| m.identifier)
                        .collect();
                    if !candidates.is_empty() {
                        out.push(Suggestion {
                            by: id.clone(),
                            relationship,
                            candidates,
                            recommended,
                        });
                    }
                }
            }
        }
        out
    }
}

fn deps_satisfied(state: &BTreeMap<String, Entry>, id: &str) -> bool {
    first_missing(state, id).is_none()
}

fn first_missing(state: &BTreeMap<String, Entry>, id: &str) -> Option<String> {
    let e = state.get(id)?;
    e.module
        .depends
        .iter()
        .find(|rel| !state.values().any(|o| o.satisfies(rel)))
        .map(|rel| rel.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests::module;

    fn dep(name: &str) -> Relationship {
        Relationship::Direct(Direct::named(name))
    }

    fn index() -> Index {
        let mut a = module("A", "1.0");
        a.depends = vec![dep("B"), dep("Virt")];
        a.recommends = vec![dep("R")];
        let mut a2 = module("A", "2.0");
        a2.depends = vec![dep("B")];
        a2.ksp_version = GameVersion::parse("1.13");
        let b = module("B", "1.0");
        let mut p1 = module("P1", "1");
        p1.provides = vec!["Virt".into()];
        let mut p2 = module("P2", "1");
        p2.provides = vec!["Virt".into()];
        let mut c = module("C", "1");
        c.conflicts = vec![dep("B")];
        let r = module("R", "1");
        Index::from_modules(vec![a, a2, b, p1, p2, c, r], HashMap::new(), 0)
    }

    fn ids(plan: &Plan) -> Vec<String> {
        plan.changes.iter().map(|c| c.to_string()).collect()
    }

    #[test]
    fn installs_with_dependencies_and_choices() {
        let index = index();
        let compat = [GameVersion::parse("1.12.5").expect("valid")];
        let r = Resolver {
            index: &index,
            compat: &compat,
            allow_testing: false,
        };
        let mut req = Request {
            install: vec![("A".into(), None)],
            ..Request::default()
        };
        let err = r.resolve(&[], &req).expect_err("needs a choice");
        let ResolveError::Choice {
            relationship,
            candidates,
            ..
        } = err
        else {
            panic!("wrong error {err:?}");
        };
        assert_eq!(candidates, ["P1", "P2"]);
        req.choices.insert(relationship, "P2".into());
        let plan = r.resolve(&[], &req).expect("resolves");
        assert_eq!(
            ids(&plan),
            [
                "install A 1.0 (requested)",
                "install B 1.0 (needed by A)",
                "install P2 1 (needed by A)",
            ]
        );
        assert_eq!(plan.suggestions.len(), 1);
        assert_eq!(plan.suggestions[0].candidates, ["R"]);
    }

    #[test]
    fn removes_dependents_and_orphans() {
        let index = index();
        let compat = [GameVersion::parse("1.12.5").expect("valid")];
        let r = Resolver {
            index: &index,
            compat: &compat,
            allow_testing: false,
        };
        let present = |id: &str, v: &str, auto: bool| Present {
            module: index
                .get(id, &ModVersion::new(v))
                .expect("in index")
                .clone(),
            auto_installed: auto,
            is_dlc: false,
            unknown_version: false,
        };
        let installed = vec![
            present("A", "1.0", false),
            present("B", "1.0", true),
            present("P1", "1", true),
        ];
        let req = Request {
            remove: vec!["B".into()],
            ..Request::default()
        };
        let plan = r.resolve(&installed, &req).expect("resolves");
        assert_eq!(
            ids(&plan),
            [
                "remove B 1.0 (requested)",
                "remove A 1.0 (depends on B)",
                "remove P1 1 (no longer needed)",
            ]
        );
    }

    #[test]
    fn detects_conflicts_and_missing() {
        let index = index();
        let compat = [GameVersion::parse("1.12.5").expect("valid")];
        let r = Resolver {
            index: &index,
            compat: &compat,
            allow_testing: false,
        };
        let req = Request {
            install: vec![("B".into(), None), ("C".into(), None)],
            ..Request::default()
        };
        assert!(matches!(
            r.resolve(&[], &req),
            Err(ResolveError::Conflict { .. })
        ));
        let req = Request {
            install: vec![("Nope".into(), None)],
            ..Request::default()
        };
        assert_eq!(
            r.resolve(&[], &req).expect_err("missing"),
            ResolveError::NotFound("Nope".into())
        );
        // A DLC satisfies dependencies with an unknown version.
        let mut needs_dlc = module("D", "1");
        needs_dlc.depends = vec![Relationship::Direct(Direct {
            name: "BreakingGround-DLC".into(),
            version: None,
            min_version: Some(ModVersion::new("1.5")),
            max_version: None,
        })];
        let index = Index::from_modules(vec![needs_dlc], HashMap::new(), 0);
        let r = Resolver {
            index: &index,
            compat: &compat,
            allow_testing: false,
        };
        let req = Request {
            install: vec![("D".into(), None)],
            ..Request::default()
        };
        let plan = r
            .resolve(&[Present::dlc("BreakingGround-DLC", None)], &req)
            .expect("dlc satisfies");
        assert_eq!(ids(&plan), ["install D 1 (requested)"]);
    }
}
