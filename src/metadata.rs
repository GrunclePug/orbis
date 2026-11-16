//! CKAN module metadata (`.ckan` files), as described in CKAN's Spec.md.
//!
//! Only the fields orbis uses are modelled; unknown fields are ignored.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::version::{GameRange, GameVersion, ModVersion};

/// One release of one mod.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Module {
    pub identifier: String,
    #[serde(default)]
    pub name: String,
    #[serde(rename = "abstract", default)]
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub version: ModVersion,
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub author: Vec<String>,
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub license: Vec<String>,
    #[serde(default)]
    pub kind: Kind,
    #[serde(default)]
    pub release_status: ReleaseStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ksp_version: Option<GameVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ksp_version_min: Option<GameVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ksp_version_max: Option<GameVersion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends: Vec<Relationship>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommends: Vec<Relationship>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggests: Vec<Relationship>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supports: Vec<Relationship>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<Relationship>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<String>,
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub download: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_hash: Option<DownloadHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<Vec<InstallDirective>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resources: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaced_by: Option<Direct>,
}

/// What sort of module this is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A normal mod with a download.
    #[default]
    Package,
    /// No download: only relationships.
    Metapackage,
    /// A paid expansion, detected rather than installed.
    Dlc,
}

/// How stable a release is. Orbis offers only `stable` unless an instance
/// opts into testing releases.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseStatus {
    #[default]
    Stable,
    Testing,
    Development,
}

/// Hashes of the download, used to verify the cached archive.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DownloadHash {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha1: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// A relationship to another module: one name, or a choice between several.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Relationship {
    AnyOf {
        any_of: Vec<Relationship>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        choice_help_text: Option<String>,
    },
    Direct(Direct),
}

/// A relationship to one module name, with optional version bounds.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Direct {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<ModVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_version: Option<ModVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_version: Option<ModVersion>,
}

impl Direct {
    /// A bare relationship to `name` with no version bounds.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: None,
            min_version: None,
            max_version: None,
        }
    }

    /// Whether a module version satisfies the bounds. `None` (an unknown
    /// version, such as an undetectable DLC version) always does.
    pub fn version_ok(&self, v: Option<&ModVersion>) -> bool {
        let Some(v) = v else { return true };
        if let Some(want) = &self.version {
            return want == v;
        }
        self.min_version.as_ref().is_none_or(|min| min <= v)
            && self.max_version.as_ref().is_none_or(|max| max >= v)
    }

    /// Whether this names a module (by identifier, with bounds) or a
    /// virtual it provides (CKAN can't check versions of provides).
    pub fn matches(
        &self,
        identifier: &str,
        version: Option<&ModVersion>,
        provides: &[String],
    ) -> bool {
        if identifier == self.name {
            self.version_ok(version)
        } else {
            provides.iter().any(|p| p == &self.name)
        }
    }
}

impl fmt::Display for Direct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if let Some(v) = &self.version {
            write!(f, " = {v}")?;
        }
        if let Some(v) = &self.min_version {
            write!(f, " >= {v}")?;
        }
        if let Some(v) = &self.max_version {
            write!(f, " <= {v}")?;
        }
        Ok(())
    }
}

impl Relationship {
    /// The direct alternatives this relationship accepts (one for a plain
    /// relationship, several for `any_of`).
    pub fn alternatives(&self) -> Vec<&Direct> {
        match self {
            Relationship::Direct(d) => vec![d],
            Relationship::AnyOf { any_of, .. } => {
                any_of.iter().flat_map(|r| r.alternatives()).collect()
            }
        }
    }

    /// Whether any alternative matches the given module.
    pub fn matches(
        &self,
        identifier: &str,
        version: Option<&ModVersion>,
        provides: &[String],
    ) -> bool {
        self.alternatives()
            .iter()
            .any(|d| d.matches(identifier, version, provides))
    }
}

impl fmt::Display for Relationship {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let alts = self.alternatives();
        for (i, d) in alts.iter().enumerate() {
            if i > 0 {
                f.write_str(" | ")?;
            }
            write!(f, "{d}")?;
        }
        Ok(())
    }
}

/// One install stanza: which part of the archive goes where.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallDirective {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub find: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub find_regexp: Option<String>,
    #[serde(default = "default_install_to")]
    pub install_to: String,
    #[serde(rename = "as", default, skip_serializing_if = "Option::is_none")]
    pub as_name: Option<String>,
    #[serde(
        default,
        deserialize_with = "opt_one_or_many",
        skip_serializing_if = "Option::is_none"
    )]
    pub filter: Option<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "opt_one_or_many",
        skip_serializing_if = "Option::is_none"
    )]
    pub filter_regexp: Option<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "opt_one_or_many",
        skip_serializing_if = "Option::is_none"
    )]
    pub include_only: Option<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "opt_one_or_many",
        skip_serializing_if = "Option::is_none"
    )]
    pub include_only_regexp: Option<Vec<String>>,
}

fn default_install_to() -> String {
    "GameData".into()
}

impl InstallDirective {
    /// CKAN's default when a module has no `install` section: the topmost
    /// folder named after the identifier, into GameData.
    pub fn default_for(identifier: &str) -> Self {
        Self {
            find: Some(identifier.to_owned()),
            install_to: default_install_to(),
            ..Self::default()
        }
    }
}

impl Module {
    /// The display name, falling back to the identifier.
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.identifier
        } else {
            &self.name
        }
    }

    /// The game versions this release claims to support.
    pub fn game_range(&self) -> Option<GameRange> {
        if let Some(v) = &self.ksp_version {
            return Some(v.range());
        }
        let lo = self
            .ksp_version_min
            .map_or(GameRange::ANY.lo, |v| v.range().lo);
        let hi = self
            .ksp_version_max
            .map_or(GameRange::ANY.hi, |v| v.range().hi);
        if lo > hi {
            return None;
        }
        Some(GameRange { lo, hi })
    }

    /// Whether this release supports any of the given game versions.
    pub fn compatible_with(&self, versions: &[GameVersion]) -> bool {
        let Some(range) = self.game_range() else {
            return false;
        };
        versions.iter().any(|v| v.range().intersects(&range))
    }

    /// A short "1.8–1.12" description of the supported game versions.
    pub fn game_range_label(&self) -> String {
        if let Some(v) = &self.ksp_version {
            return v.to_string();
        }
        match (&self.ksp_version_min, &self.ksp_version_max) {
            (None, None) => "any".into(),
            (Some(min), None) => format!("{min}+"),
            (None, Some(max)) => format!("<= {max}"),
            (Some(min), Some(max)) if min == max => min.to_string(),
            (Some(min), Some(max)) => format!("{min}–{max}"),
        }
    }

    /// The install stanzas to use, applying CKAN's default.
    pub fn directives(&self) -> Vec<InstallDirective> {
        match &self.install {
            Some(d) if !d.is_empty() => d.clone(),
            _ => vec![InstallDirective::default_for(&self.identifier)],
        }
    }

    /// CKAN's archive.org mirror of the download, offered only for licenses
    /// that allow redistribution (CKAN uploads those automatically).
    pub fn mirror_url(&self) -> Option<String> {
        if !self
            .license
            .iter()
            .any(|l| REDISTRIBUTABLE.contains(&l.as_str()))
        {
            return None;
        }
        let hash = self
            .download_hash
            .as_ref()
            .and_then(|h| h.sha1.as_deref().or(h.sha256.as_deref()))?;
        let raw = format!("{}-{}", self.identifier, self.version.as_str())
            .replace(' ', "_")
            .replace(':', "-");
        let bucket: String = raw
            .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            .take(100)
            .collect();
        let prefix: String = hash.chars().take(8).collect();
        if bucket.is_empty() || prefix.len() < 8 {
            return None;
        }
        Some(format!(
            "https://archive.org/download/{bucket}/{prefix}-{bucket}.zip"
        ))
    }

    /// The first string resource with the given key (homepage, repository...).
    pub fn resource(&self, key: &str) -> Option<&str> {
        self.resources.get(key).and_then(|v| v.as_str())
    }
}

/// Licenses CKAN may redistribute (`redistributable_license` in CKAN.schema).
const REDISTRIBUTABLE: [&str; 84] = [
    "public-domain",
    "Apache",
    "Apache-1.0",
    "Apache-2.0",
    "Artistic",
    "Artistic-1.0",
    "Artistic-2.0",
    "BSD-2-clause",
    "BSD-3-clause",
    "BSD-4-clause",
    "ISC",
    "CC-BY",
    "CC-BY-1.0",
    "CC-BY-2.0",
    "CC-BY-2.5",
    "CC-BY-3.0",
    "CC-BY-4.0",
    "CC-BY-SA",
    "CC-BY-SA-1.0",
    "CC-BY-SA-2.0",
    "CC-BY-SA-2.5",
    "CC-BY-SA-3.0",
    "CC-BY-SA-4.0",
    "CC-BY-NC",
    "CC-BY-NC-1.0",
    "CC-BY-NC-2.0",
    "CC-BY-NC-2.5",
    "CC-BY-NC-3.0",
    "CC-BY-NC-4.0",
    "CC-BY-NC-SA",
    "CC-BY-NC-SA-1.0",
    "CC-BY-NC-SA-2.0",
    "CC-BY-NC-SA-2.5",
    "CC-BY-NC-SA-3.0",
    "CC-BY-NC-SA-4.0",
    "CC-BY-NC-ND",
    "CC-BY-NC-ND-1.0",
    "CC-BY-NC-ND-2.0",
    "CC-BY-NC-ND-2.5",
    "CC-BY-NC-ND-3.0",
    "CC-BY-NC-ND-4.0",
    "CC-BY-ND",
    "CC-BY-ND-1.0",
    "CC-BY-ND-2.0",
    "CC-BY-ND-2.5",
    "CC-BY-ND-3.0",
    "CC-BY-ND-4.0",
    "CC0",
    "CDDL",
    "CPL",
    "EFL-1.0",
    "EFL-2.0",
    "Expat",
    "MIT",
    "GPL-1.0",
    "GPL-2.0",
    "GPL-3.0",
    "LGPL-2.0",
    "LGPL-2.1",
    "LGPL-3.0",
    "GFDL-1.0",
    "GFDL-1.1",
    "GFDL-1.2",
    "GFDL-1.3",
    "GFDL-NIV-1.0",
    "GFDL-NIV-1.1",
    "GFDL-NIV-1.2",
    "GFDL-NIV-1.3",
    "LPPL-1.0",
    "LPPL-1.1",
    "LPPL-1.2",
    "LPPL-1.3c",
    "MPL-1.1",
    "MPL-2.0",
    "Perl",
    "Python-2.0",
    "QPL-1.0",
    "W3C",
    "Zlib",
    "Zope",
    "WTFPL",
    "Unlicense",
    "open-source",
    "unrestricted",
];

fn one_or_many<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(opt_one_or_many(d)?.unwrap_or_default())
}

fn opt_one_or_many<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
        Null(()),
    }
    match OneOrMany::deserialize(d) {
        Ok(OneOrMany::One(s)) => Ok(Some(vec![s])),
        Ok(OneOrMany::Many(v)) => Ok(Some(v)),
        Ok(OneOrMany::Null(())) => Ok(None),
        Err(e) => Err(de::Error::custom(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "spec_version": "v1.4",
        "identifier": "Scatterer",
        "name": "Scatterer",
        "abstract": "Atmospheric scattering",
        "author": "blackrack",
        "license": ["GPL-3.0"],
        "version": "2:v0.0838",
        "ksp_version_min": "1.8",
        "ksp_version_max": "1.12",
        "depends": [
            { "name": "ModuleManager", "min_version": "4.0" },
            { "any_of": [ { "name": "Kopernicus" }, { "name": "KopernicusBE" } ] }
        ],
        "provides": ["Scatterer-config"],
        "download": "https://example.invalid/scatterer.zip",
        "download_hash": { "sha1": "AB", "sha256": "CD" },
        "install": [ { "find": "scatterer", "install_to": "GameData", "filter": "Thumbs.db" } ],
        "resources": { "homepage": "https://example.invalid", "x_custom": { "a": 1 } }
    }"#;

    #[test]
    fn parses_sample() {
        let m: Module = serde_json::from_str(SAMPLE).expect("sample parses");
        assert_eq!(m.author, vec!["blackrack"]);
        assert_eq!(m.depends.len(), 2);
        assert_eq!(m.depends[1].alternatives().len(), 2);
        assert_eq!(
            m.install.as_ref().expect("install")[0].filter,
            Some(vec!["Thumbs.db".into()])
        );
        assert_eq!(m.resource("homepage"), Some("https://example.invalid"));
        let v = |s| GameVersion::parse(s).expect("valid");
        assert!(m.compatible_with(&[v("1.12.5")]));
        assert!(!m.compatible_with(&[v("1.7.3")]));
        assert_eq!(m.game_range_label(), "1.8–1.12");
        let round = serde_json::to_string(&m).expect("serializes");
        let back: Module = serde_json::from_str(&round).expect("round trip");
        assert_eq!(back.version, m.version);
    }

    #[test]
    fn mirror_urls_follow_ckan() {
        let mut m: Module = serde_json::from_value(serde_json::json!({
            "identifier": "FelineUtilityRovers",
            "version": "1.3.6",
            "license": "CC-BY-NC",
            "download_hash": { "sha1": "EEBEA0EB357588CACEF24B627EDDF32A4F4BBE2F" }
        }))
        .expect("parses");
        assert_eq!(
            m.mirror_url().as_deref(),
            Some(
                "https://archive.org/download/FelineUtilityRovers-1.3.6/EEBEA0EB-FelineUtilityRovers-1.3.6.zip"
            )
        );
        m.version = ModVersion::new("2:v 1.0+b");
        assert_eq!(
            m.mirror_url().as_deref(),
            Some(
                "https://archive.org/download/FelineUtilityRovers-2-v_1.0b/EEBEA0EB-FelineUtilityRovers-2-v_1.0b.zip"
            )
        );
        m.license = vec!["restricted".into()];
        assert_eq!(m.mirror_url(), None);
    }

    #[test]
    fn relationship_matching() {
        let d = Direct {
            name: "ModuleManager".into(),
            version: None,
            min_version: Some(ModVersion::new("4.0")),
            max_version: None,
        };
        assert!(d.matches("ModuleManager", Some(&ModVersion::new("4.2.3")), &[]));
        assert!(!d.matches("ModuleManager", Some(&ModVersion::new("3.1")), &[]));
        assert!(d.matches(
            "Other",
            Some(&ModVersion::new("1.0")),
            &["ModuleManager".into()]
        ));
        assert!(d.matches("ModuleManager", None, &[]));
    }
}
