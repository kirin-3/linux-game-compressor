use std::{
    io,
    path::Path,
    process::{Command, Output},
};
fn command(args: &[&str]) -> io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_compressor"))
        .args(args)
        .output()
}
fn str_path(path: &Path) -> &str {
    path.to_str().unwrap()
}
#[test]
fn incompatible_plans_and_manifests_do_not_change_output() -> io::Result<()> {
    let fixture = compressor_test_fixtures::Fixture::new()?;
    let tmp = tempfile::tempdir()?;
    let plan = tmp.path().join("old-plan.json");
    let out = tmp.path().join("output");
    std::fs::write(&plan, br#"{"plan_version":3}"#)?;
    let args = [
        "compress",
        str_path(fixture.root()),
        "--output",
        str_path(&out),
        "--plan",
        str_path(&plan),
    ];
    let result = command(&args)?;
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("incompatible version; re-run plan"));
    assert!(!out.exists());
    std::fs::create_dir_all(out.join(".compressor"))?;
    let manifest = out.join(".compressor/manifest.json");
    let old = br#"{"plan":{"plan_version":3}}"#;
    std::fs::write(&manifest, old)?;
    std::fs::write(out.join("keep"), b"unchanged")?;
    let result = command(&[
        "compress",
        str_path(fixture.root()),
        "--output",
        str_path(&out),
    ])?;
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("incompatible version; re-run plan"));
    assert_eq!(std::fs::read(manifest)?, old);
    assert_eq!(std::fs::read(out.join("keep"))?, b"unchanged");
    assert_eq!(std::fs::read_dir(out.join(".compressor"))?.count(), 1);
    Ok(())
}
#[test]
fn plan_audio_overrides_and_saved_plan_conflict() -> io::Result<()> {
    let fixture = compressor_test_fixtures::Fixture::new()?;
    let path = fixture.root().join("game/music.ogg");
    let status = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "anoisesrc=duration=2",
            "-ac",
            "2",
            "-c:a",
            "libvorbis",
            "-q:a",
            "8",
            "-y",
        ])
        .arg(&path)
        .status()?;
    assert!(status.success());
    let result = command(&[
        "plan",
        str_path(fixture.root()),
        "--set",
        "audio.vorbis_quality=2",
        "--set",
        "engines.renpy.ogg_target=vorbis",
        "--json",
    ])?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&result.stdout)?;
    let entry = json["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "game/music.ogg")
        .unwrap();
    assert_eq!(entry["action"]["settings"]["audio"]["codec"], "vorbis");
    assert_eq!(entry["action"]["settings"]["audio"]["vorbis_quality"], 2);
    let dir = tempfile::tempdir()?;
    let out = dir.path().join("output");
    let result = command(&[
        "compress",
        str_path(fixture.root()),
        "--output",
        str_path(&out),
        "--plan",
        "plan.json",
        "--set",
        "image.quality=70",
    ])?;
    assert_eq!(result.status.code(), Some(2));
    assert!(!out.exists());
    Ok(())
}
#[test]
fn cancellation_stops_running_ffmpeg_and_removes_stage() -> io::Result<()> {
    use std::{
        os::unix::fs::PermissionsExt,
        process::Stdio,
        thread,
        time::{Duration, Instant},
    };
    let fixture = compressor_test_fixtures::Fixture::new()?;
    let audio = fixture.root().join("game/music.ogg");
    let status = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "anoisesrc=duration=10",
            "-ac",
            "2",
            "-c:a",
            "libvorbis",
            "-q:a",
            "8",
            "-y",
        ])
        .arg(audio)
        .status()?;
    assert!(status.success());
    let dir = tempfile::tempdir()?;
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin)?;
    let wrapper = bin.join("ffmpeg");
    std::fs::write(&wrapper, "#!/bin/sh\nif [ \"$1\" = -nostdin ]; then echo $$ > \"$AUDIO_TEST_PID\"; exec /usr/bin/ffmpeg -re \"$@\"; fi\nexec /usr/bin/ffmpeg \"$@\"\n")?;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))?;
    let pids = dir.path().join("pid");
    let output = dir.path().join("out");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_compressor"))
        .args([
            "compress",
            str_path(fixture.root()),
            "--output",
            str_path(&output),
            "--jobs",
            "1",
        ])
        .env("PATH", path)
        .env("AUDIO_TEST_PID", &pids)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let until = Instant::now() + Duration::from_secs(20);
    while !pids.exists() && Instant::now() < until {
        if let Some(status) = child.try_wait()? {
            panic!("compressor exited before encoding: {status}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(pids.exists(), "ffmpeg did not start");
    let pid: i32 = std::fs::read_to_string(&pids)?.trim().parse().unwrap();
    assert!(Path::new(&format!("/proc/{pid}")).exists());
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let status = child.wait()?;
    assert_eq!(status.code(), Some(130));
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "ffmpeg survived cancellation"
    );
    assert_eq!(
        std::fs::read_dir(output.join(".compressor/tmp"))?.count(),
        0
    );
    Ok(())
}
#[test]
fn settings_command_reports_sources_and_rejects_invalid_values() -> io::Result<()> {
    let result = command(&[
        "settings",
        "--engine",
        "renpy",
        "--profile",
        "small",
        "--set",
        "image.quality=70",
    ])?;
    assert!(result.status.success());
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(
        output.contains("image.quality = 70 (source: --set)"),
        "{output}"
    );
    assert!(
        output.contains(
            "engines.renpy.ogg_target = opus (source: engine default; allowed: opus, vorbis)"
        ),
        "{output}"
    );
    assert!(
        output.contains("audio.vorbis_quality = 1 (source: profile small)"),
        "{output}"
    );
    let fixture = compressor_test_fixtures::Fixture::new()?;
    let result = command(&["settings", str_path(fixture.root()), "--json"])?;
    assert!(result.status.success());
    let json: serde_json::Value = serde_json::from_slice(&result.stdout)?;
    assert_eq!(json["engine"], "renpy");
    assert_eq!(json["settings"]["values"]["image.quality"]["value"], 82);
    let result = command(&[
        "settings",
        "--engine",
        "renpy",
        "--set",
        "image.quality=150",
    ])?;
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr)
        .contains("image.quality: must be an integer between 0 and 100"));
    Ok(())
}
#[test]
fn inspect_and_plan_report_audio_and_tools() -> io::Result<()> {
    let fixture = compressor_test_fixtures::Fixture::new()?;
    let path = fixture.root().join("game/music.ogg");
    let status = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "anoisesrc=duration=2",
            "-ac",
            "2",
            "-c:a",
            "libvorbis",
            "-q:a",
            "8",
            "-y",
        ])
        .arg(&path)
        .status()?;
    assert!(status.success());
    let result = command(&["inspect", str_path(fixture.root()), "--json"])?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&result.stdout)?;
    assert_eq!(json["media"]["vorbis"]["count"], 1);
    assert_eq!(
        json["media"]["vorbis"]["bytes"],
        std::fs::metadata(path)?.len()
    );
    let tools = json["tools"].as_array().unwrap();
    let ffmpeg = tools.iter().find(|v| v[0] == "ffmpeg").unwrap();
    assert_eq!(ffmpeg[1], true);
    assert_eq!(ffmpeg[2]["libopus"], true);
    assert!(tools.iter().any(|v| v[0] == "ffprobe"));
    let plan = command(&["plan", str_path(fixture.root())])?;
    assert!(String::from_utf8_lossy(&plan.stdout).contains("audio convert: 1"));
    Ok(())
}
#[test]
fn verifies_mixed_output_and_detects_modified_audio() -> io::Result<()> {
    let fixture = compressor_test_fixtures::Fixture::new()?;
    let audio = fixture.root().join("game/music.ogg");
    let status = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "anoisesrc=duration=2",
            "-ac",
            "2",
            "-c:a",
            "libvorbis",
            "-q:a",
            "8",
            "-y",
        ])
        .arg(audio)
        .status()?;
    assert!(status.success());
    let dir = tempfile::tempdir()?;
    let output = dir.path().join("out");
    let result = command(&[
        "compress",
        str_path(fixture.root()),
        "--output",
        str_path(&output),
    ])?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(output.join(".compressor/manifest.json"))?)?;
    let record = manifest["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "game/music.ogg")
        .unwrap();
    assert!(
        record["outcome"].as_str().unwrap().starts_with("converted"),
        "{record}"
    );
    let result = command(&["verify", str_path(&output)])?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    std::fs::write(output.join("game/music.ogg"), b"changed")?;
    let result = command(&["verify", str_path(&output)])?;
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("game/music.ogg: hash mismatch"));
    Ok(())
}
#[test]
fn inspect_plan_compress_verify_and_reject_conflicts() -> io::Result<()> {
    let fixture = compressor_test_fixtures::Fixture::new()?;
    let game = str_path(fixture.root());
    let temp = tempfile::tempdir()?;
    let plan = temp.path().join("plan.json");
    let out = temp.path().join("output");
    let inspect = command(&["inspect", game, "--json"])?;
    assert!(
        inspect.status.success(),
        "{}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&inspect.stdout)?;
    assert_eq!(result["engine"]["version"], "8.5.0");
    assert!(result["media"]["png"]["count"].as_u64().unwrap() > 0);
    assert!(result["archives"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a[0].as_str().unwrap().contains("archive.rpa")));
    assert!(result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool[0] == "cwebp"));
    let planning = command(&[
        "plan",
        game,
        "--profile",
        "balanced",
        "--save",
        str_path(&plan),
        "--json",
    ])?;
    assert!(
        planning.status.success(),
        "{}",
        String::from_utf8_lossy(&planning.stderr)
    );
    let plan_json: serde_json::Value = serde_json::from_slice(&planning.stdout)?;
    let entries = plan_json["entries"].as_array().unwrap();
    let convert = entries
        .iter()
        .filter(|e| e["action"]["type"] == "convert")
        .count();
    let listed = command(&["plan", game, "--profile", "balanced", "--list"])?;
    assert!(listed.status.success());
    let summary = String::from_utf8_lossy(&listed.stdout);
    assert!(summary.contains(&format!("convert: {convert}")));
    for entry in entries {
        let path = entry["path"].as_str().unwrap();
        assert!(summary.contains(path), "missing {path}");
    }
    let conflicting = command(&[
        "compress",
        game,
        "--output",
        str_path(&out),
        "--profile",
        "balanced",
        "--plan",
        str_path(&plan),
    ])?;
    assert_eq!(conflicting.status.code(), Some(2));
    assert!(!out.exists());
    let compress = command(&[
        "compress",
        game,
        "--output",
        str_path(&out),
        "--plan",
        str_path(&plan),
        "--jobs",
        "2",
    ])?;
    assert!(
        compress.status.success(),
        "{}",
        String::from_utf8_lossy(&compress.stderr)
    );
    assert!(!compress.stdout.contains(&27));
    assert!(!compress.stderr.contains(&27));
    assert_eq!(
        std::fs::read(out.join("game/archive.rpa"))?,
        std::fs::read(fixture.root().join("game/archive.rpa"))?
    );
    assert_eq!(
        std::fs::read(out.join("game/script.rpyc"))?,
        std::fs::read(fixture.root().join("game/script.rpyc"))?
    );
    assert_eq!(
        std::fs::read(out.join("game/images/animated.png"))?,
        std::fs::read(fixture.root().join("game/images/animated.png"))?
    );
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join(".compressor/report.json"))?)?;
    let skips = report["skips"].as_array().unwrap();
    for (path, reason) in [
        ("game/icon.png", "loaded outside image pipeline"),
        ("game/images/existing.webp", "already WebP"),
        ("game/images/mislabelled.png", "unsupported source format"),
        ("game/images/animated.png", "animated image"),
    ] {
        assert!(
            skips
                .iter()
                .any(|e| e["path"] == path && e["outcome"].as_str().unwrap().contains(reason)),
            "{path}: {reason}"
        );
    }
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join(".compressor/manifest.json"))?)?;
    assert!(manifest["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["outcome"].as_str().unwrap().starts_with("converted")));
    for entry in manifest["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["outcome"].as_str().unwrap().starts_with("converted"))
    {
        let name = entry["path"].as_str().unwrap();
        let bytes = std::fs::read(out.join(name))?;
        assert!(bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP");
    }
    std::fs::rename(
        fixture.root(),
        fixture.root().with_file_name("relocated-input"),
    )?;
    let verify = command(&["verify", str_path(&out), "--json"])?;
    assert!(
        verify.status.success(),
        "{}",
        String::from_utf8_lossy(&verify.stdout)
    );
    let result: serde_json::Value = serde_json::from_slice(&verify.stdout)?;
    assert!(result["errors"].as_array().unwrap().is_empty());
    std::fs::write(out.join("game/script.rpyc"), b"tampered")?;
    let modified = command(&["verify", str_path(&out), "--json"])?;
    assert!(!modified.status.success());
    let result: serde_json::Value = serde_json::from_slice(&modified.stdout)?;
    assert!(!result["errors"].as_array().unwrap().is_empty());
    Ok(())
}

#[test]
fn interrupts_stop_encoder_group_and_second_interrupt_is_immediate() -> io::Result<()> {
    use std::{
        os::unix::fs::{symlink, PermissionsExt},
        process::Stdio,
        thread,
        time::{Duration, Instant},
    };
    let fixture = compressor_test_fixtures::Fixture::new()?;
    let temp = tempfile::tempdir()?;
    let tools = temp.path().join("bin");
    std::fs::create_dir(&tools)?;
    let encoder = tools.join("cwebp");
    std::fs::write(&encoder, "#!/bin/sh\nif [ \"$1\" = -version ]; then echo fake-1; exit 0; fi\n/usr/bin/sleep 60 &\nc=$!\necho $$ $c > \"$COMPRESSOR_TEST_PIDS\"\ntrap 'kill $c 2>/dev/null; wait $c 2>/dev/null; while :; do :; done' TERM\nwait $c\n")?;
    std::fs::set_permissions(&encoder, std::fs::Permissions::from_mode(0o755))?;
    symlink("/usr/bin/dwebp", tools.join("dwebp"))?;
    let output = temp.path().join("output");
    let pids_file = temp.path().join("pids");
    let mut child = Command::new(env!("CARGO_BIN_EXE_compressor"))
        .args([
            "compress",
            str_path(fixture.root()),
            "--output",
            str_path(&output),
            "--jobs",
            "1",
        ])
        .env("PATH", &tools)
        .env("COMPRESSOR_TEST_PIDS", &pids_file)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let until = Instant::now() + Duration::from_secs(10);
    while !pids_file.exists() && Instant::now() < until {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(pids_file.exists(), "encoder did not start");
    let pids: Vec<i32> = std::fs::read_to_string(&pids_file)?
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect();
    assert_eq!(pids.len(), 2);
    let started = Instant::now();
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let until = Instant::now() + Duration::from_secs(1);
    while Path::new(&format!("/proc/{}", pids[1])).exists() && Instant::now() < until {
        thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let until = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= until {
            child.kill()?;
            panic!("compressor did not stop");
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(130));
    assert!(
        started.elapsed() < Duration::from_millis(1900),
        "second interrupt did not force immediate termination"
    );
    for pid in pids {
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "process {pid} survived"
        );
    }
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(output.join(".compressor/report.json"))?)?;
    assert_eq!(report["complete"], false);
    assert_eq!(
        std::fs::read_dir(output.join(".compressor/tmp"))?.count(),
        0
    );
    Ok(())
}

#[test]
fn failed_encoder_returns_one_and_keeps_original() -> io::Result<()> {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let fixture = compressor_test_fixtures::Fixture::new()?;
    let temp = tempfile::tempdir()?;
    let tools = temp.path().join("bin");
    std::fs::create_dir(&tools)?;
    let encoder = tools.join("cwebp");
    std::fs::write(&encoder,"#!/bin/sh\nif [ \"$1\" = -version ]; then echo fake-1; exit 0; fi\necho encoder-failed >&2\nexit 1\n")?;
    std::fs::set_permissions(&encoder, std::fs::Permissions::from_mode(0o755))?;
    symlink("/usr/bin/dwebp", tools.join("dwebp"))?;
    let output = temp.path().join("output");
    let run = Command::new(env!("CARGO_BIN_EXE_compressor"))
        .args([
            "compress",
            str_path(fixture.root()),
            "--output",
            str_path(&output),
            "--jobs",
            "2",
        ])
        .env("PATH", tools)
        .output()?;
    assert_eq!(
        run.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(
        std::fs::read(output.join("game/images/opaque.png"))?,
        std::fs::read(fixture.root().join("game/images/opaque.png"))?
    );
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(output.join(".compressor/report.json"))?)?;
    assert!(!report["failures"].as_array().unwrap().is_empty());
    Ok(())
}

#[test]
fn documented_usage_commands_run() -> io::Result<()> {
    use std::os::unix::fs::symlink;
    let fixture = compressor_test_fixtures::Fixture::new()?;
    let temp = tempfile::tempdir()?;
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin)?;
    symlink(env!("CARGO_BIN_EXE_compressor"), bin.join("compressor"))?;
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let plan = temp.path().join("plan.json");
    let output = temp.path().join("output");
    let readme =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../README.md"))?;
    let commands = readme
        .split("```sh\n")
        .nth(1)
        .unwrap()
        .split("```")
        .next()
        .unwrap();
    for line in commands.lines().filter(|line| !line.trim().is_empty()) {
        let result = Command::new("/bin/sh")
            .args(["-c", line])
            .env("PATH", &path)
            .env("GAME", fixture.root())
            .env("PLAN", &plan)
            .env("OUTPUT", &output)
            .output()?;
        assert!(
            result.status.success(),
            "{line}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    Ok(())
}

#[test]
fn godot_plan_lists_pack_members() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut header = [0; 36];
    header[..4].copy_from_slice(b"GST2");
    let texture = compressor_engine_godot::ctex::Texture {
        header,
        data_format: 1,
        width: 16,
        height: 16,
        mipmaps: 0,
        image_format: 5,
        images: vec![vec![42; 5000]],
    }
    .write()?;
    let pack = compressor_test_fixtures::godot_pack_members(&[
        ("asset.ctex", &texture),
        ("scene.gdc", b"script"),
    ]);
    std::fs::write(dir.path().join("game.pck"), pack)?;
    let result = command(&["plan", str_path(dir.path()), "--list"])?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let documented = command(&[
        "plan",
        str_path(dir.path()),
        "--set",
        "engines.godot.min_texture_kib=4",
        "--list",
    ])?;
    assert!(
        documented.status.success(),
        "{}",
        String::from_utf8_lossy(&documented.stderr)
    );
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(stdout.contains("game.pck::asset.ctex"));
    assert!(stdout.contains("game.pck::scene.gdc"));
    assert!(stdout.contains("Rebuild"));
    assert!(stdout.contains("packs rebuilt: 1"));
    assert!(stdout.contains("image convert: 1"));
    Ok(())
}
#[test]
fn inspect_reports_godot_pack_and_encryption() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let pack = dir.path().join("game.pck");
    std::fs::write(
        &pack,
        compressor_test_fixtures::godot_pack_members(&[
            ("texture.ctex", b"texture"),
            ("music.mp3str", b"audio"),
            ("scene.gdc", b"script"),
        ]),
    )?;
    let result = command(&["inspect", str_path(dir.path()), "--json"])?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&result.stdout)?;
    assert_eq!(json["engine"]["version"], "4.7.0");
    assert_eq!(json["packs"][0]["location"], "standalone");
    assert_eq!(json["packs"][0]["format"], 4);
    assert_eq!(json["packs"][0]["members"]["texture"]["count"], 1);
    std::fs::write(&pack, compressor_test_fixtures::godot_pack(4, 1))?;
    let result = command(&["inspect", str_path(dir.path()), "--json"])?;
    assert!(result.status.success());
    let json: serde_json::Value = serde_json::from_slice(&result.stdout)?;
    assert_eq!(json["packs"][0]["encrypted"], true);
    assert!(json["packs"][0]["note"]
        .as_str()
        .unwrap()
        .contains("copied unchanged"));
    Ok(())
}
#[test]
fn verifies_godot_member_and_reports_tampering() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let game = dir.path().join("game");
    let output = dir.path().join("output");
    std::fs::create_dir(&game)?;
    let png = dir.path().join("image.png");
    let mut img = image::RgbaImage::new(128, 128);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = image::Rgba([
            (x * 2) as u8,
            (y * 2) as u8,
            ((x * y) % 256) as u8,
            if x < 20 { 0 } else { 255 },
        ]);
    }
    img.save(&png).map_err(io::Error::other)?;
    let mut header = [0; 36];
    header[..4].copy_from_slice(b"GST2");
    let texture = compressor_engine_godot::ctex::Texture {
        header,
        data_format: 1,
        width: 128,
        height: 128,
        mipmaps: 0,
        image_format: 5,
        images: vec![std::fs::read(png)?],
    }
    .write()?;
    std::fs::write(
        game.join("game.pck"),
        compressor_test_fixtures::godot_pack_members(&[("texture.ctex", &texture)]),
    )?;
    let result = command(&["compress", str_path(&game), "--output", str_path(&output)])?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result = command(&["verify", str_path(&output)])?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let replacement = dir.path().join("bad");
    std::fs::write(&replacement, b"altered")?;
    let pack = output.join("game.pck");
    let changed = dir.path().join("changed");
    compressor_engine_godot::rebuild_pack(
        &pack,
        &changed,
        &std::collections::BTreeMap::from([("texture.ctex".into(), replacement)]),
    )?;
    std::fs::rename(changed, pack)?;
    let result = command(&["verify", str_path(&output)])?;
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("game.pck::texture.ctex: hash mismatch")
    );
    Ok(())
}

#[test]
fn inspect_rpgmaker_encryption_video_and_enigma() -> io::Result<()> {
    let fixture = compressor_test_fixtures::RpgMakerFixture::new(false, true)?;
    let video = fixture.content().join("movies/clip.mp4");
    assert!(Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x48:rate=10:duration=1",
            "-c:v",
            "libx264",
            "-y"
        ])
        .arg(&video)
        .status()?
        .success());
    let result = command(&["inspect", str_path(fixture.root()), "--json"])?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&result.stdout)?;
    assert_eq!(json["engine"]["name"], "RPG Maker MV");
    assert_eq!(json["rpgmaker"]["content_root"], "www");
    assert_eq!(json["rpgmaker"]["key_source"], "key from System.json");
    assert_eq!(json["rpgmaker"]["video_count"], 1);
    assert!(json["rpgmaker"]["encrypted_count"].as_u64().unwrap() >= 4);
    assert!(json["tools"].to_string().contains("libvpx-vp9"));
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("Game.exe"), b"")?;
    std::fs::write(dir.path().join("nw.dll"), b"")?;
    std::fs::create_dir(dir.path().join("locales"))?;
    let result = command(&["inspect", str_path(dir.path())])?;
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Enigma Virtual Box"));
    Ok(())
}

#[test]
fn plan_reports_removed_sizes_and_envelopes() -> io::Result<()> {
    let fixture = compressor_test_fixtures::RpgMakerFixture::new(true, true)?;
    let removed_size = std::fs::metadata(fixture.content().join("audio/bgm/sample.m4a_"))?.len();
    let result = command(&["plan", str_path(fixture.root()), "--list"])?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let text = String::from_utf8_lossy(&result.stdout);
    assert!(
        text.contains(&format!(
            "remove: 1 files, {removed_size} bytes (unused on desktop)"
        )),
        "{text}"
    );
    assert!(text.contains("lossless: true"));
    assert!(text.contains("rpgmaker"));
    Ok(())
}

#[test]
fn verify_checks_encrypted_conversions_removals_and_video() -> io::Result<()> {
    let fixture = compressor_test_fixtures::RpgMakerFixture::new(true, true)?;
    let temp = tempfile::tempdir()?;
    let png = temp.path().join("large.png");
    image::RgbaImage::from_pixel(512, 512, image::Rgba([15, 80, 200, 255]))
        .save(&png)
        .map_err(io::Error::other)?;
    std::fs::write(
        fixture.content().join("img/pictures/large.png_"),
        compressor_test_fixtures::RpgMakerFixture::encrypt(&std::fs::read(png)?),
    )?;
    let video = fixture.content().join("movies/clip.avi");
    assert!(Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=128x96:rate=10:duration=1",
            "-c:v",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            "-y"
        ])
        .arg(&video)
        .status()?
        .success());
    let out = temp.path().join("output");
    let result = command(&[
        "compress",
        str_path(fixture.root()),
        "--output",
        str_path(&out),
        "--jobs",
        "1",
    ])?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join(".compressor/manifest.json"))?)?;
    let entries = manifest["entries"].as_array().unwrap();
    for name in ["img/pictures/large.png_", "movies/clip.avi"] {
        assert!(
            entries.iter().any(|v| v["path"] == name
                && v["outcome"].as_str().unwrap_or("").starts_with("converted")),
            "{name}: {manifest}"
        );
    }
    let verified = command(&["verify", str_path(&out)])?;
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
    std::fs::write(out.join("audio/bgm/sample.m4a_"), b"restored")?;
    let failed = command(&["verify", str_path(&out)])?;
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("removed file exists"));
    Ok(())
}

#[test]
fn documented_commands_run_against_rpgmaker_fixture() -> io::Result<()> {
    let fixture = compressor_test_fixtures::RpgMakerFixture::new(true, true)?;
    let temp = tempfile::tempdir()?;
    let game = str_path(fixture.root());
    let plan_path = temp.path().join("plan.json");
    let out = temp.path().join("output");
    let commands: Vec<Vec<&str>> = vec![
        vec!["inspect", game],
        vec!["inspect", game, "--json"],
        vec![
            "settings",
            "--engine",
            "renpy",
            "--profile",
            "small",
            "--set",
            "image.quality=70",
        ],
        vec!["settings", game, "--json"],
        vec!["plan", game, "--profile", "balanced", "--list"],
        vec![
            "plan",
            game,
            "--profile",
            "small",
            "--set",
            "audio.vorbis_quality=2",
            "--save",
            str_path(&plan_path),
        ],
        vec![
            "compress",
            game,
            "--output",
            str_path(&out),
            "--plan",
            str_path(&plan_path),
            "--jobs",
            "2",
        ],
        vec!["verify", str_path(&out)],
        vec!["verify", str_path(&out), "--json"],
        vec![
            "plan",
            game,
            "--set",
            "engines.godot.min_texture_kib=4",
            "--list",
        ],
    ];
    for args in commands {
        let result = command(&args)?;
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    Ok(())
}
