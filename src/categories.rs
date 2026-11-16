//! Mod categories: worked out from CKAN tags, or set by the user (also to
//! categories of their own), shared by every instance.

use std::collections::BTreeMap;
use std::fs;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::{self, Paths};

/// The built-in categories, in the order they're listed.
pub const BUILT_IN: [&str; 14] = [
    "Parts",
    "Visuals",
    "Planets",
    "Communications",
    "Career & science",
    "Tools & info",
    "KSC & launch sites",
    "Crew & IVA",
    "Resources",
    "Physics",
    "Sound",
    "Patches & configs",
    "Libraries",
    "Other",
];

/// The category CKAN tags point to. The first rule that matches wins, so a
/// library tagged `parts` stays a library and a parts pack with a sound
/// config stays parts.
pub fn from_tags(tags: &[String]) -> &'static str {
    let has = |names: &[&str]| tags.iter().any(|t| names.contains(&t.as_str()));
    let rules: [(&[&str], &str); 13] = [
        (&["library"], "Libraries"),
        (&["planet-pack"], "Planets"),
        (&["graphics"], "Visuals"),
        (&["sound"], "Sound"),
        (&["buildings"], "KSC & launch sites"),
        (&["comms"], "Communications"),
        (
            &["career", "tech-tree", "science", "agency"],
            "Career & science",
        ),
        (&["parts"], "Parts"),
        (
            &["information", "control", "convenience", "editor"],
            "Tools & info",
        ),
        (&["physics"], "Physics"),
        (&["resources"], "Resources"),
        (&["crewed", "first-person", "suits"], "Crew & IVA"),
        (&["config"], "Patches & configs"),
    ];
    rules
        .iter()
        .find(|(t, _)| has(t))
        .map_or("Other", |(_, c)| c)
}

/// Where a category sorts: built-in ones in their order, the user's own
/// before Libraries and Other, alphabetically.
pub fn rank(category: &str) -> (usize, String) {
    let tail = BUILT_IN.len() - 2;
    match BUILT_IN.iter().position(|c| *c == category) {
        Some(i) if i >= tail => (i + 1, String::new()),
        Some(i) => (i, String::new()),
        None => (tail, category.to_lowercase()),
    }
}

/// The user's category choices, stored in `<data>/categories.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Categories {
    /// Mod identifier -> category.
    #[serde(default)]
    pub mods: BTreeMap<String, String>,
}

impl Categories {
    /// Loads the choices (none when the file doesn't exist yet).
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.data.join("categories.toml");
        match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parse {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Writes the choices.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        let text = toml::to_string_pretty(self).context("serialize categories")?;
        paths::write_atomic(&paths.data.join("categories.toml"), text.as_bytes())
    }

    /// A mod's category: the user's choice, else what its tags say.
    pub fn of(&self, id: &str, tags: &[String]) -> String {
        self.mods
            .get(id)
            .cloned()
            .unwrap_or_else(|| from_tags(tags).to_owned())
    }

    /// Sets a mod's category; `None`, or the category its tags already give,
    /// goes back to automatic.
    pub fn set(&mut self, id: &str, tags: &[String], category: Option<&str>) {
        match category.map(str::trim).filter(|c| !c.is_empty()) {
            Some(c) if c != from_tags(tags) => {
                self.mods.insert(id.to_owned(), c.to_owned());
            }
            _ => {
                self.mods.remove(id);
            }
        }
    }

    /// Categories the user made up, sorted.
    pub fn custom(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .mods
            .values()
            .filter(|c| !BUILT_IN.contains(&c.as_str()))
            .cloned()
            .collect();
        v.sort_by_key(|c| c.to_lowercase());
        v.dedup();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tags_pick_a_category() {
        let cases: [(&[&str], &str); 8] = [
            (&["plugin", "library", "career"], "Libraries"),
            (&["plugin", "sound", "crewed"], "Sound"),
            (&["parts", "crewed"], "Parts"),
            (&["config", "career", "uncrewed"], "Career & science"),
            (&["plugin", "information", "comms"], "Communications"),
            (&["config"], "Patches & configs"),
            (&["plugin"], "Other"),
            (&[], "Other"),
        ];
        for (t, want) in cases {
            assert_eq!(from_tags(&tags(t)), want, "{t:?}");
        }
    }

    #[test]
    fn overrides_and_custom_categories() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(Some(tmp.path())).expect("paths");
        let mut c = Categories::load(&paths).expect("empty");
        let rover = tags(&["parts", "crewed"]);
        assert_eq!(c.of("FelineUtilityRovers", &rover), "Parts");
        c.set("FelineUtilityRovers", &rover, Some("Rovers"));
        c.set("KAS", &tags(&["parts"]), Some("Parts"));
        assert!(!c.mods.contains_key("KAS"), "same as automatic");
        c.save(&paths).expect("save");
        let mut back = Categories::load(&paths).expect("load");
        assert_eq!(back.of("FelineUtilityRovers", &rover), "Rovers");
        assert_eq!(back.custom(), ["Rovers"]);
        back.set("FelineUtilityRovers", &rover, None);
        assert_eq!(back.of("FelineUtilityRovers", &rover), "Parts");
    }

    #[test]
    fn custom_categories_sort_before_libraries() {
        let mut v = vec!["Other", "Rovers", "Libraries", "Parts", "Boats"];
        v.sort_by_key(|c| rank(c));
        assert_eq!(v, ["Parts", "Boats", "Rovers", "Libraries", "Other"]);
    }
}
