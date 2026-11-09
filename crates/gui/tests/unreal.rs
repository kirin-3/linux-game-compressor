use compressor_test_fixtures::iostore::{self, Chunk, Options};
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn command(args: &[&str]) -> io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_linux-game-compressor"))
        .args(args)
        .output()
}
fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}
fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

fn chunks() -> Vec<Chunk> {
    (0..40u8)
        .map(|i| Chunk {
            kind: if i % 10 == 0 { 9 } else { 2 },
            data: iostore::sample_bytes(i, 7000 + 50 * i as usize),
        })
        .collect()
}

fn write_container(paks: &Path, name: &str, options: &Options) -> io::Result<()> {
    fs::create_dir_all(paks)?;
    let built = iostore::build(&chunks(), options);
    fs::write(paks.join(format!("{name}.utoc")), built.utoc)?;
    fs::write(paks.join(format!("{name}.ucas")), built.ucas)
}

/// UNREAL_2 style: a raw game container, a global container, a pak, and an engine
/// executable that names UE 5.5.
fn raw_game() -> io::Result<tempfile::TempDir> {
    let temp = tempfile::tempdir()?;
    let game = temp.path().join("game");
    let paks = game.join("Game/EscapeFrom/Content/Paks");
    write_container(&paks, "EscapeFrom-Windows", &Options::default())?;
    write_container(&paks, "global", &Options::default())?;
    fs::write(paks.join("EscapeFrom-Windows.pak"), b"synthetic pak")?;
    let exe = game.join("Game/Engine/Binaries/Win64");
    fs::create_dir_all(&exe)?;
    fs::write(
        exe.join("UnrealGame-Win64-Shipping.exe"),
        b"MZ ++UE5+Release-5.5-CL-40574608 ",
    )?;
    Ok(temp)
}

/// UNREAL_1 style: a container that already uses Oodle.
fn compressed_game() -> io::Result<tempfile::TempDir> {
    let temp = tempfile::tempdir()?;
    let game = temp.path().join("game");
    let options = Options {
        flags: iostore::FLAG_COMPRESSED | iostore::FLAG_INDEXED,
        method_names: vec!["Oodle".into()],
        ..Options::default()
    };
    write_container(
        &game.join("Windows/prealpha/Content/Paks"),
        "prealpha-Windows",
        &options,
    )?;
    let exe = game.join("Windows/prealpha/Binaries/Win64");
    fs::create_dir_all(&exe)?;
    fs::write(
        exe.join("prealpha.exe"),
        b"MZ 5.7.4-++UE5+Release-5.7-CL-51494982 ",
    )?;
    fs::create_dir_all(game.join("Windows/Engine"))?;
    Ok(temp)
}

fn game_dir(temp: &tempfile::TempDir) -> PathBuf {
    temp.path().join("game")
}

#[test]
fn inspect_reports_raw_and_compressed_containers() -> io::Result<()> {
    let raw = raw_game()?;
    let result = command(&["inspect", p(&game_dir(&raw))])?;
    assert!(result.status.success(), "{}", text(&result));
    let out = text(&result);
    assert!(out.contains("Unreal Engine 5.5"), "{out}");
    let line = out
        .lines()
        .find(|l| l.contains("EscapeFrom-Windows"))
        .expect("container line");
    assert!(
        line.contains("uncompressed") && line.contains("not encrypted"),
        "{line}"
    );
    assert!(line.contains("will be compressed"), "{line}");
    let global = out
        .lines()
        .find(|l| l.contains("/global"))
        .expect("global line");
    assert!(
        global.contains("copied: engine global container"),
        "{global}"
    );

    let json = command(&["inspect", p(&game_dir(&raw)), "--json"])?;
    let value: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    let containers = value["unreal"].as_array().unwrap();
    assert_eq!(containers.len(), 2);
    let game = containers
        .iter()
        .find(|c| c["name"] == "EscapeFrom-Windows")
        .unwrap();
    assert_eq!(game["will_compress"], true);
    assert_eq!(game["toc_version"], 8);

    let old = compressed_game()?;
    let out = text(&command(&["inspect", p(&game_dir(&old))])?);
    assert!(out.contains("Unreal Engine 5.7.4"), "{out}");
    let line = out
        .lines()
        .find(|l| l.contains("prealpha-Windows"))
        .unwrap();
    assert!(line.contains("compressed with Oodle"), "{line}");
    assert!(
        line.contains("copied: already compressed container"),
        "{line}"
    );
    Ok(())
}

#[test]
fn plan_counts_and_lists_rewritten_pairs() -> io::Result<()> {
    let raw = raw_game()?;
    let game = game_dir(&raw);
    let summary = text(&command(&["plan", p(&game)])?);
    assert!(
        summary
            .lines()
            .any(|l| l.starts_with("pairs rewritten: 1 (")),
        "{summary}"
    );
    assert!(summary.contains("rewrite: unreal-iostore: 1"), "{summary}");
    let list = text(&command(&["plan", p(&game), "--list"])?);
    let paks = "Game/EscapeFrom/Content/Paks/EscapeFrom-Windows";
    assert!(
        list.contains(&format!("{paks}.ucas: rewrite: unreal-iostore")),
        "{list}"
    );
    assert!(
        list.contains(&format!("{paks}.utoc: rewrite partner: {paks}.ucas")),
        "{list}"
    );
    assert!(
        list.contains("global.ucas: Copy { reason: \"engine global container\" }"),
        "{list}"
    );
    let off = text(&command(&[
        "plan",
        p(&game),
        "--set",
        "engines.unreal.compress_containers=false",
    ])?);
    assert!(
        off.lines().any(|l| l.starts_with("pairs rewritten: 0")),
        "{off}"
    );
    assert!(off.contains("copy: compression disabled"), "{off}");
    Ok(())
}

#[test]
fn verify_checks_rewritten_and_kept_pairs() -> io::Result<()> {
    let raw = raw_game()?;
    let game = game_dir(&raw);
    let out = raw.path().join("out");
    let result = command(&["compress", p(&game), "--output", p(&out)])?;
    assert!(result.status.success(), "{}", text(&result));
    let paks = out.join("Game/EscapeFrom/Content/Paks");
    let input =
        fs::metadata(game.join("Game/EscapeFrom/Content/Paks/EscapeFrom-Windows.ucas"))?.len();
    assert!(fs::metadata(paks.join("EscapeFrom-Windows.ucas"))?.len() < input);
    assert_eq!(
        fs::read(paks.join("global.ucas"))?,
        fs::read(game.join("Game/EscapeFrom/Content/Paks/global.ucas"))?
    );
    let clean = command(&["verify", p(&out)])?;
    assert!(clean.status.success(), "{}", text(&clean));

    for (file, offset, expected) in [
        ("EscapeFrom-Windows.ucas", 3usize, "EscapeFrom-Windows.ucas"),
        (
            "EscapeFrom-Windows.utoc",
            200usize,
            "EscapeFrom-Windows.utoc",
        ),
    ] {
        let path = paks.join(file);
        let original = fs::read(&path)?;
        let mut changed = original.clone();
        changed[offset] ^= 0x01;
        fs::write(&path, changed)?;
        let bad = command(&["verify", p(&out)])?;
        assert!(!bad.status.success(), "{file}");
        assert!(
            text(&bad).contains(&format!("{expected}: hash mismatch")),
            "{}",
            text(&bad)
        );
        fs::write(&path, original)?;
    }
    assert!(command(&["verify", p(&out)])?.status.success());

    // A pair kept as the original is still checked against the input hashes.
    let old = compressed_game()?;
    let kept = old.path().join("out");
    let result = command(&["compress", p(&game_dir(&old)), "--output", p(&kept)])?;
    assert!(result.status.success(), "{}", text(&result));
    assert!(command(&["verify", p(&kept)])?.status.success());
    let ucas = kept.join("Windows/prealpha/Content/Paks/prealpha-Windows.ucas");
    let mut bytes = fs::read(&ucas)?;
    bytes[5] ^= 0xFF;
    fs::write(&ucas, bytes)?;
    let bad = command(&["verify", p(&kept)])?;
    assert!(!bad.status.success());
    assert!(
        text(&bad).contains("prealpha-Windows.ucas: hash mismatch"),
        "{}",
        text(&bad)
    );
    Ok(())
}
