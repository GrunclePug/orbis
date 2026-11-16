//! A reader for Valve's text KeyValues format (`.vdf`), enough for Steam's
//! `libraryfolders.vdf`, `config.vdf` and `compatibilitytool.vdf`.

use anyhow::{Result, bail};

/// A VDF value: a string, or an ordered list of key/value pairs.
#[derive(Clone, Debug, PartialEq)]
pub enum Vdf {
    Str(String),
    Obj(Vec<(String, Vdf)>),
}

impl Vdf {
    /// Parses a whole document. The top level is a list of key/value pairs.
    pub fn parse(text: &str) -> Result<Vdf> {
        let tokens = tokenize(text)?;
        let mut pos = 0;
        let obj = parse_pairs(&tokens, &mut pos, false)?;
        Ok(Vdf::Obj(obj))
    }

    /// The child with this key (case-insensitive, like Steam).
    pub fn get(&self, key: &str) -> Option<&Vdf> {
        match self {
            Vdf::Obj(pairs) => pairs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            Vdf::Str(_) => None,
        }
    }

    /// Follows a path of keys.
    pub fn path(&self, keys: &[&str]) -> Option<&Vdf> {
        keys.iter().try_fold(self, |v, k| v.get(k))
    }

    /// The string value, if this is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Vdf::Str(s) => Some(s),
            Vdf::Obj(_) => None,
        }
    }

    /// The key/value pairs, if this is an object.
    pub fn pairs(&self) -> &[(String, Vdf)] {
        match self {
            Vdf::Obj(p) => p,
            Vdf::Str(_) => &[],
        }
    }
}

#[derive(Debug, PartialEq)]
enum Tok {
    Str(String),
    Open,
    Close,
}

fn tokenize(text: &str) -> Result<Vec<Tok>> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '{' => out.push(Tok::Open),
            '}' => out.push(Tok::Close),
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '"' => {
                let mut s = String::new();
                loop {
                    match chars.next() {
                        None => bail!("unterminated string"),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some(other) => s.push(other),
                            None => bail!("unterminated escape"),
                        },
                        Some(other) => s.push(other),
                    }
                }
                out.push(Tok::Str(s));
            }
            _ => {
                let mut s = String::from(c);
                while let Some(&n) = chars.peek() {
                    if n.is_whitespace() || n == '{' || n == '}' || n == '"' {
                        break;
                    }
                    s.push(n);
                    chars.next();
                }
                out.push(Tok::Str(s));
            }
        }
    }
    Ok(out)
}

fn parse_pairs(tokens: &[Tok], pos: &mut usize, nested: bool) -> Result<Vec<(String, Vdf)>> {
    let mut pairs = Vec::new();
    loop {
        match tokens.get(*pos) {
            None if nested => bail!("missing closing brace"),
            None => return Ok(pairs),
            Some(Tok::Close) if nested => {
                *pos += 1;
                return Ok(pairs);
            }
            Some(Tok::Close) => bail!("unexpected closing brace"),
            Some(Tok::Open) => bail!("unexpected opening brace"),
            Some(Tok::Str(key)) => {
                *pos += 1;
                let value = match tokens.get(*pos) {
                    Some(Tok::Str(v)) => {
                        *pos += 1;
                        Vdf::Str(v.clone())
                    }
                    Some(Tok::Open) => {
                        *pos += 1;
                        Vdf::Obj(parse_pairs(tokens, pos, true)?)
                    }
                    _ => bail!("key {key:?} has no value"),
                };
                pairs.push((key.clone(), value));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_with_comments() {
        let text = r#"
"libraryfolders"
{
    "0" // first library
    {
        "path"      "/home/user/.local/share/Steam"
        "apps" { "220200" "5911193735" }
    }
    unquoted value
}
"#;
        let v = Vdf::parse(text).expect("parses");
        let lib = v.path(&["LibraryFolders", "0"]).expect("library 0");
        assert_eq!(
            lib.get("path").and_then(Vdf::as_str),
            Some("/home/user/.local/share/Steam")
        );
        assert!(lib.path(&["apps", "220200"]).is_some());
        assert_eq!(
            v.path(&["libraryfolders", "unquoted"])
                .and_then(Vdf::as_str),
            Some("value")
        );
    }

    #[test]
    fn rejects_broken_input() {
        assert!(Vdf::parse("\"a\" {").is_err());
        assert!(Vdf::parse("\"a\"").is_err());
    }
}
