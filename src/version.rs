//! Version types: CKAN module versions and KSP game versions.
//!
//! Module version ordering is ported from CKAN's `ModuleVersion` (MIT,
//! the CKAN authors; see NOTICE) and follows it exactly (an
//! optional `epoch:` prefix, then alternating string and number runs), so
//! "latest version" means the same thing in orbis as it does in CKAN.

use std::cmp::Ordering;
use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A CKAN module version such as `1:v2.3.1` or `1.12.0.2`.
#[derive(Clone, Debug)]
pub struct ModVersion(String);

impl ModVersion {
    /// Wraps a version string. Every string is a valid CKAN version.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The version exactly as written in the metadata.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn split_epoch(&self) -> (u64, &str) {
        let s = self.0.as_str();
        if let Some((head, rest)) = s.split_once(':')
            && !head.is_empty()
            && head.bytes().all(|b| b.is_ascii_digit())
        {
            return (head.parse().unwrap_or(0), rest);
        }
        (0, s)
    }
}

impl fmt::Display for ModVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq for ModVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ModVersion {}

impl PartialOrd for ModVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ModVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let (e1, v1) = self.split_epoch();
        let (e2, v2) = other.split_epoch();
        if e1 != e2 {
            return e1.cmp(&e2);
        }
        if v1 == v2 {
            return Ordering::Equal;
        }
        compare_ckan(v1, v2)
    }
}

/// CKAN's version-ordering dance (Spec.md, "Version ordering").
fn compare_ckan(a: &str, b: &str) -> Ordering {
    let mut r1 = a;
    let mut r2 = b;
    while !r1.is_empty() && !r2.is_empty() {
        let (s1, rest1) = split_at_first(r1, |c| c.is_ascii_digit());
        let (s2, rest2) = split_at_first(r2, |c| c.is_ascii_digit());
        let ord = compare_string_part(s1, s2);
        if ord != Ordering::Equal {
            return ord;
        }
        r1 = rest1;
        r2 = rest2;

        let (n1, rest1) = split_at_first(r1, |c| !c.is_ascii_digit());
        let (n2, rest2) = split_at_first(r2, |c| !c.is_ascii_digit());
        let ord = n1
            .parse::<u64>()
            .unwrap_or(0)
            .cmp(&n2.parse::<u64>().unwrap_or(0));
        if ord != Ordering::Equal {
            return ord;
        }
        r1 = rest1;
        r2 = rest2;
    }
    match (r1.is_empty(), r2.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        _ => Ordering::Greater,
    }
}

/// Splits `s` before the first char matching `pred` (or returns all of `s`).
fn split_at_first(s: &str, pred: impl Fn(char) -> bool) -> (&str, &str) {
    match s.find(pred) {
        Some(i) => s.split_at(i),
        None => (s, ""),
    }
}

/// Compares the non-numeric runs. A '.' sorts above any other character.
fn compare_string_part(s1: &str, s2: &str) -> Ordering {
    match (s1.chars().next(), s2.chars().next()) {
        (Some(c1), Some(c2)) => match (c1 == '.', c2 == '.') {
            (false, true) => Ordering::Less,
            (true, false) => Ordering::Greater,
            (true, true) => {
                if s1.len() == 1 && s2.len() > 1 {
                    Ordering::Greater
                } else if s1.len() > 1 && s2.len() == 1 {
                    Ordering::Less
                } else {
                    Ordering::Equal
                }
            }
            (false, false) => s1.cmp(s2),
        },
        _ => s1.cmp(s2),
    }
}

impl Serialize for ModVersion {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ModVersion {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(LooseString).map(ModVersion)
    }
}

/// Accepts a JSON string or number as a string (old metadata uses both).
struct LooseString;

impl Visitor<'_> for LooseString {
    type Value = String;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string or number")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<String, E> {
        Ok(v.to_owned())
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<String, E> {
        Ok(v.to_string())
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<String, E> {
        Ok(v.to_string())
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<String, E> {
        Ok(v.to_string())
    }
}

/// A KSP version with up to four parts (`1.12.5.3190`). Missing parts are
/// wildcards: `1.12` covers every 1.12.x release, and no parts means "any".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct GameVersion {
    parts: [Option<u32>; 4],
}

/// An inclusive range of fully specified game versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GameRange {
    pub lo: [u32; 4],
    pub hi: [u32; 4],
}

impl GameRange {
    /// Every game version.
    pub const ANY: GameRange = GameRange {
        lo: [0; 4],
        hi: [u32::MAX; 4],
    };

    /// Whether two ranges share at least one version.
    pub fn intersects(&self, other: &GameRange) -> bool {
        self.lo.max(other.lo) <= self.hi.min(other.hi)
    }
}

impl GameVersion {
    /// The wildcard version that matches every release.
    pub const ANY: GameVersion = GameVersion { parts: [None; 4] };

    /// Builds a version from major.minor.patch.
    pub fn new(major: u32, minor: u32, patch: Option<u32>) -> Self {
        Self {
            parts: [Some(major), Some(minor), patch, None],
        }
    }

    /// Parses `1.12.5`, `1.12`, `any` or an empty string. Returns `None` for
    /// anything else.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("any") {
            return Some(Self::ANY);
        }
        let mut parts = [None; 4];
        for (i, piece) in s.split('.').enumerate() {
            if i >= 4 {
                return None;
            }
            parts[i] = Some(piece.parse().ok()?);
        }
        Some(Self { parts })
    }

    /// Whether this is the "any" wildcard.
    pub fn is_any(&self) -> bool {
        self.parts[0].is_none()
    }

    /// The same version without the build number (`1.12.5.3190` -> `1.12.5`).
    pub fn without_build(&self) -> Self {
        let mut parts = self.parts;
        parts[3] = None;
        Self { parts }
    }

    /// Major.minor only (`1.12.5` -> `1.12`).
    pub fn major_minor(&self) -> Self {
        Self {
            parts: [self.parts[0], self.parts[1], None, None],
        }
    }

    /// Major part, if set.
    pub fn major(&self) -> Option<u32> {
        self.parts[0]
    }

    /// Minor part, if set.
    pub fn minor(&self) -> Option<u32> {
        self.parts[1]
    }

    /// The versions this one stands for: missing parts span their full range.
    pub fn range(&self) -> GameRange {
        let mut lo = [0; 4];
        let mut hi = [u32::MAX; 4];
        for (i, p) in self.parts.iter().enumerate() {
            match p {
                Some(v) => {
                    lo[i] = *v;
                    hi[i] = *v;
                }
                None => break,
            }
        }
        GameRange { lo, hi }
    }
}

impl fmt::Display for GameVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_any() {
            return f.write_str("any");
        }
        let mut first = true;
        for p in self.parts.iter().map_while(|p| *p) {
            if !first {
                f.write_str(".")?;
            }
            write!(f, "{p}")?;
            first = false;
        }
        Ok(())
    }
}

impl PartialOrd for GameVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GameVersion {
    /// Orders by parts, with a missing part sorting before any number.
    fn cmp(&self, other: &Self) -> Ordering {
        self.parts.cmp(&other.parts)
    }
}

impl Serialize for GameVersion {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for GameVersion {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = d.deserialize_any(LooseString)?;
        GameVersion::parse(&s).ok_or_else(|| de::Error::custom(format!("bad game version {s:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_version_ordering() {
        let cases = [
            ("1.0", "1.1", Ordering::Less),
            ("1.2", "1.2.3", Ordering::Less),
            ("1.10", "1.9", Ordering::Greater),
            ("1.0", "1.00", Ordering::Equal),
            ("1:0.1", "2.0", Ordering::Greater),
            ("v1.2", "v1.3", Ordering::Less),
            ("1.2a", "1.2.1", Ordering::Less),
            ("1.2-beta", "1.2", Ordering::Greater),
            ("2.0.0", "2.0.0", Ordering::Equal),
            ("1.12.5.3190", "1.12.5", Ordering::Greater),
            ("0.9.9", "0.10", Ordering::Less),
        ];
        for (a, b, want) in cases {
            let got = ModVersion::new(a).cmp(&ModVersion::new(b));
            assert_eq!(got, want, "{a} vs {b}");
            assert_eq!(
                ModVersion::new(b).cmp(&ModVersion::new(a)),
                want.reverse(),
                "{b} vs {a}"
            );
        }
    }

    #[test]
    fn game_version_parse_and_ranges() {
        let v = GameVersion::parse("1.12.5").expect("valid");
        assert_eq!(v.to_string(), "1.12.5");
        assert!(GameVersion::parse("any").expect("valid").is_any());
        assert!(GameVersion::parse("1.x").is_none());

        let cases = [
            ("1.12", "1.12.5", true),
            ("1.11", "1.12.5", false),
            ("any", "1.12.5", true),
            ("1.12.5", "1.12", true),
            ("1.12.3", "1.12.5", false),
        ];
        for (a, b, want) in cases {
            let a = GameVersion::parse(a).expect("valid").range();
            let b = GameVersion::parse(b).expect("valid").range();
            assert_eq!(a.intersects(&b), want, "{a:?} {b:?}");
        }
    }
}
