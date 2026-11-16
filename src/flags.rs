//! Custom flags: a library of PNG images shared by every instance.
//!
//! The library is `<data>/flags/`. Each instance's GameData gets an
//! `OrbisFlags` folder whose `Flags` entry is a symlink to the library, so a
//! flag added once shows up in every instance's flag picker, and KSP knows it
//! as `OrbisFlags/Flags/<name>`.

use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::paths::{Paths, slug};

/// The GameData folder orbis owns in every instance for the flag library.
pub const FOLDER: &str = "OrbisFlags";

/// A flag in the library.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flag {
    /// The file name without `.png`; KSP shows it as `OrbisFlags/Flags/<name>`.
    pub name: String,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
}

/// The library directory.
pub fn dir(paths: &Paths) -> PathBuf {
    paths.data.join("flags")
}

/// The flags in the library, sorted by name. Files that aren't readable PNGs
/// are skipped with a warning.
pub fn list(paths: &Paths) -> Result<Vec<Flag>> {
    let dir = dir(paths);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    let mut out = Vec::new();
    for e in entries {
        let path = e.with_context(|| format!("read {}", dir.display()))?.path();
        let is_png = path
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("png"));
        let Some(name) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        if !is_png || name.starts_with('.') {
            continue;
        }
        match size(&path) {
            Ok((width, height)) => out.push(Flag {
                name,
                path,
                width,
                height,
            }),
            Err(e) => log::warn!("{}: {e:#}", path.display()),
        }
    }
    out.sort_by_key(|f| f.name.to_lowercase());
    Ok(out)
}

/// Copies a PNG into the library as `<name>.png`, where `name` defaults to
/// the file's own name. Fails if the file isn't a PNG or the name is taken.
pub fn add(paths: &Paths, src: &Path, name: Option<&str>) -> Result<Flag> {
    let (width, height) =
        size(src).with_context(|| format!("{} is not a PNG image", src.display()))?;
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = slug(name.filter(|n| !n.trim().is_empty()).unwrap_or(&stem));
    let dir = dir(paths);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{name}.png"));
    if path.exists() {
        bail!("a flag named {name} already exists");
    }
    let tmp = dir.join(format!(".{name}.png.tmp"));
    fs::copy(src, &tmp).with_context(|| format!("copy {}", src.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("move into {}", path.display()))?;
    Ok(Flag {
        name,
        path,
        width,
        height,
    })
}

/// Deletes a flag from the library. Saves that use it fall back to KSP's
/// default flag.
pub fn remove(paths: &Paths, name: &str) -> Result<()> {
    let path = dir(paths).join(format!("{name}.png"));
    if !path.is_file() {
        bail!("no flag named {name}");
    }
    fs::remove_file(&path).with_context(|| format!("delete {}", path.display()))
}

/// Makes `game/GameData/OrbisFlags/Flags` a link to the library, creating
/// the library when it doesn't exist yet. Fails if something other than a
/// link is in the way.
pub fn link(paths: &Paths, game: &Path) -> Result<()> {
    let lib = dir(paths);
    fs::create_dir_all(&lib).with_context(|| format!("create {}", lib.display()))?;
    let parent = game.join("GameData").join(FOLDER);
    fs::create_dir_all(&parent).with_context(|| format!("create {}", parent.display()))?;
    let link = parent.join("Flags");
    match fs::symlink_metadata(&link) {
        Ok(m) if m.file_type().is_symlink() => {
            if fs::read_link(&link).ok().as_deref() == Some(lib.as_path()) {
                return Ok(());
            }
            fs::remove_file(&link).with_context(|| format!("replace {}", link.display()))?;
        }
        Ok(_) => bail!(
            "{} should be a link to the flag library; move it away",
            link.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("stat {}", link.display())),
    }
    std::os::unix::fs::symlink(&lib, &link).with_context(|| format!("link {}", link.display()))
}

fn decoder(path: &Path) -> Result<png::Reader<BufReader<File>>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut dec = png::Decoder::new(BufReader::new(file));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    dec.read_info().context("read PNG header")
}

/// The image's width and height.
pub fn size(path: &Path) -> Result<(u32, u32)> {
    let reader = decoder(path)?;
    let info = reader.info();
    Ok((info.width, info.height))
}

/// Decodes a PNG to 8-bit RGBA pixels, for showing it.
pub fn rgba(path: &Path) -> Result<(u32, u32, Vec<u8>)> {
    let mut reader = decoder(path)?;
    let len = reader
        .output_buffer_size()
        .context("PNG is too large to decode")?;
    let mut buf = vec![0; len];
    let out = reader.next_frame(&mut buf).context("decode PNG")?;
    let data = &buf[..out.buffer_size()];
    let px = match out.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => data
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[g, a]| [g, g, g, a])
            .collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => bail!("PNG palette was not expanded"),
    };
    Ok((out.width, out.height, px))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_png(path: &Path, w: u32, h: u32) {
        let file = File::create(path).expect("create png");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().expect("header");
        let data = vec![200u8; (w * h * 3) as usize];
        writer.write_image_data(&data).expect("data");
    }

    #[test]
    fn adds_lists_and_removes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(Some(tmp.path())).expect("paths");
        let src = tmp.path().join("C Orbital Solutions.png");
        write_png(&src, 256, 160);
        let f = add(&paths, &src, None).expect("add");
        assert_eq!(f.name, "c-orbital-solutions");
        assert_eq!((f.width, f.height), (256, 160));
        assert!(add(&paths, &src, None).is_err(), "duplicate name");
        add(&paths, &src, Some("Second")).expect("add renamed");
        fs::write(dir(&paths).join("broken.png"), "not a png").expect("write");
        let names: Vec<_> = list(&paths)
            .expect("list")
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["c-orbital-solutions", "second"]);
        remove(&paths, "second").expect("remove");
        assert!(remove(&paths, "second").is_err());
        assert_eq!(list(&paths).expect("list").len(), 1);
    }

    #[test]
    fn refuses_non_png() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(Some(tmp.path())).expect("paths");
        let src = tmp.path().join("flag.png");
        fs::write(&src, "text").expect("write");
        assert!(add(&paths, &src, None).is_err());
    }

    #[test]
    fn decodes_to_rgba() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("f.png");
        write_png(&src, 4, 2);
        let (w, h, px) = rgba(&src).expect("decode");
        assert_eq!((w, h, px.len()), (4, 2, 32));
        assert_eq!(&px[..4], &[200, 200, 200, 255]);
    }

    #[test]
    fn links_instances_to_library() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(Some(tmp.path())).expect("paths");
        let game = tmp.path().join("game");
        fs::create_dir_all(game.join("GameData")).expect("mkdir");
        link(&paths, &game).expect("link");
        link(&paths, &game).expect("link again is a no-op");
        let l = game.join("GameData/OrbisFlags/Flags");
        assert_eq!(fs::read_link(&l).expect("is link"), dir(&paths));
        // A stale link (library moved) is replaced.
        fs::remove_file(&l).expect("rm");
        std::os::unix::fs::symlink(tmp.path().join("old"), &l).expect("stale link");
        link(&paths, &game).expect("relink");
        assert_eq!(fs::read_link(&l).expect("is link"), dir(&paths));
        // A real folder is left alone.
        fs::remove_file(&l).expect("rm");
        fs::create_dir(&l).expect("real dir");
        assert!(link(&paths, &game).is_err());
    }
}
