//! End-to-end checks of the `orbis` binary against a temporary root.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

fn orbis(root: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_orbis"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("run orbis");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[test]
fn instance_and_manual_mod_lifecycle() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ksp = tmp.path().join("ksp");
    fs::create_dir_all(ksp.join("GameData/Squad")).expect("mkdir");
    fs::write(ksp.join("KSP_x64.exe"), b"").expect("write");
    fs::write(ksp.join("readme.txt"), "Version 1.12.5\n").expect("write");
    let root = tmp.path().join("root");

    let (ok, out) = orbis(
        &root,
        &[
            "instance",
            "create",
            "Career",
            "--from",
            ksp.to_str().expect("utf8"),
        ],
    );
    assert!(ok, "{out}");
    let (ok, out) = orbis(&root, &["instance", "list"]);
    assert!(
        ok && out.contains("Career") && out.contains("1.12.5"),
        "{out}"
    );

    let zip_path = tmp.path().join("clouds.zip");
    let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).expect("create"));
    w.start_file(
        "Clouds-1/GameData/Clouds/clouds.dll",
        zip::write::SimpleFileOptions::default(),
    )
    .expect("start");
    w.write_all(b"dll").expect("write");
    w.finish().expect("finish");

    let (ok, out) = orbis(
        &root,
        &[
            "manual",
            "add",
            zip_path.to_str().expect("utf8"),
            "--name",
            "Clouds",
        ],
    );
    assert!(ok, "{out}");
    let (ok, out) = orbis(&root, &["manual", "install", "clouds", "-y"]);
    assert!(ok, "{out}");
    let game = root.join("data/instances/career/game");
    assert!(game.join("GameData/Clouds/clouds.dll").is_file());

    let (ok, out) = orbis(&root, &["remove", "clouds", "-y"]);
    assert!(ok, "{out}");
    assert!(!game.join("GameData/Clouds").exists());

    let (ok, out) = orbis(&root, &["--version"]);
    assert!(ok && out.contains(env!("CARGO_PKG_VERSION")), "{out}");
}

/// Folding a category hides its rows from the list only: they still count,
/// and a search shows them. Runs the GUI headless (needs a Vulkan driver;
/// lavapipe works).
#[test]
fn folded_categories_keep_their_rows_and_open_for_a_search() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ksp = tmp.path().join("ksp");
    fs::create_dir_all(ksp.join("GameData/Squad")).expect("mkdir");
    fs::write(ksp.join("KSP_x64.exe"), b"").expect("write");
    fs::write(ksp.join("readme.txt"), "Version 1.12.5\n").expect("write");
    let root = tmp.path().join("root");
    let ksp_arg = ksp.to_str().expect("utf8");
    let (ok, out) = orbis(&root, &["instance", "create", "Career", "--from", ksp_arg]);
    assert!(ok, "{out}");
    let zip_path = tmp.path().join("clouds.zip");
    let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).expect("create"));
    w.start_file(
        "GameData/Clouds/clouds.dll",
        zip::write::SimpleFileOptions::default(),
    )
    .expect("start");
    w.write_all(b"dll").expect("write");
    w.finish().expect("finish");
    let zip_arg = zip_path.to_str().expect("utf8");
    let (ok, out) = orbis(&root, &["manual", "add", zip_arg, "--name", "Clouds"]);
    assert!(ok, "{out}");
    let (ok, out) = orbis(&root, &["manual", "install", "clouds", "-y"]);
    assert!(ok, "{out}");
    // Manual mods have no CKAN tags, so Clouds is in Other; fold it.
    fs::create_dir_all(root.join("config")).expect("mkdir");
    fs::write(root.join("config/gui.toml"), "collapsed = [\"Other\"]\n").expect("prefs");

    let dump = tmp.path().join("dump.txt");
    let dump_arg = dump.to_str().expect("utf8");
    let base = [
        "gui",
        "--headless",
        "--frames",
        "2",
        "--tab",
        "mods",
        "--filter",
        "installed",
    ];
    let mut args: Vec<&str> = base.to_vec();
    args.extend(["--dump", dump_arg]);
    let (ok, out) = orbis(&root, &args);
    assert!(ok, "{out}");
    let text = fs::read_to_string(&dump).expect("dump");
    assert!(text.contains("rows: 1"), "folded rows still count:\n{text}");
    assert!(text.contains("groups: Other 1 (folded)"), "{text}");

    args.extend(["--search", "clouds"]);
    let (ok, out) = orbis(&root, &args);
    assert!(ok, "{out}");
    let text = fs::read_to_string(&dump).expect("dump");
    assert!(
        text.contains("groups: Other 1\n"),
        "a search opens folds:\n{text}"
    );
}
