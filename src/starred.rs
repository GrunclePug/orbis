//! Starred mods: a global list of favourites shared by every instance.

use std::collections::BTreeSet;
use std::fs;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::{self, Paths};

/// Identifiers the user has starred, stored in `<data>/starred.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Starred {
    #[serde(default)]
    pub mods: BTreeSet<String>,
}

impl Starred {
    /// Loads the starred list (empty when it doesn't exist yet).
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.data.join("starred.toml");
        match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parse {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Writes the starred list.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        let text = toml::to_string_pretty(self).context("serialize starred mods")?;
        paths::write_atomic(&paths.data.join("starred.toml"), text.as_bytes())
    }

    pub fn contains(&self, id: &str) -> bool {
        self.mods.contains(id)
    }

    /// Stars or unstars a mod; returns whether it is now starred.
    pub fn toggle(&mut self, id: &str) -> bool {
        if self.mods.remove(id) {
            false
        } else {
            self.mods.insert(id.to_owned());
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggles_and_persists() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(Some(tmp.path())).expect("paths");
        let mut s = Starred::load(&paths).expect("empty");
        assert!(s.toggle("Scatterer"));
        assert!(s.toggle("MechJeb2"));
        assert!(!s.toggle("MechJeb2"));
        s.save(&paths).expect("save");
        let back = Starred::load(&paths).expect("load");
        assert!(back.contains("Scatterer") && !back.contains("MechJeb2"));
    }
}
