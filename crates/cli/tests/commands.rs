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
