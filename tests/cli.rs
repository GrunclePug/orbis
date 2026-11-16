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
