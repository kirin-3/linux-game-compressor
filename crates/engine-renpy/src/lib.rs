//! Ren'Py game detection and classification.
use compressor_core::{
    registry::EngineAdapter, Action, EngineInfo, MediaKind, Profile, StoredPath,
};
use compressor_media::{detect, ImageFormat};
use regex::Regex;
use std::{fs, io, path::Path, sync::LazyLock};
use walkdir::WalkDir;

static VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*version\s*=\s*['\"](\d+)\.(\d+)\.(\d+)"#).unwrap());
static TUPLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)").unwrap());

static ICON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)\bconfig\.window_icon\s*=\s*['"]([^'"\r\n]+)['"]"#).unwrap()
});
pub const MIN_WEBP_VERSION: (u32, u32, u32) = (6, 99, 12);

#[derive(Default)]
pub struct RenpyAdapter {
    pub allow_unknown_version: bool,
}
impl EngineAdapter for RenpyAdapter {
    fn detect(&self, root: &Path) -> io::Result<Option<EngineInfo>> {
        if !root.join("game").is_dir() || !root.join("renpy").is_dir() {
            return Ok(None);
        }
        let parse = |file: &Path, regex: &Regex| -> Option<String> {
            let text = fs::read_to_string(file).ok()?;
            let m = regex.captures(&text)?;
            Some(format!("{}.{}.{}", &m[1], &m[2], &m[3]))
        };
        let version = parse(&root.join("renpy/vc_version.py"), &VERSION)
            .or_else(|| parse(&root.join("game/script_version.txt"), &TUPLE));
        let mut excluded_paths = Vec::new();
        let mut scripts = false;
        let mut compiled = false;
        for entry in WalkDir::new(root.join("game")).follow_links(false) {
            let entry = entry.map_err(io::Error::other)?;
            if !entry.file_type().is_file() {
                continue;
            }
            let ext = entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if ext == "rpyc" {
                compiled = true;
            }
            if ext == "rpy" {
                scripts = true;
                if let Ok(source) = fs::read_to_string(entry.path()) {
                    for found in ICON.captures_iter(&source) {
                        let path = Path::new(&found[1]);
                        if path
                            .components()
                            .all(|c| matches!(c, std::path::Component::Normal(_)))
                        {
                            excluded_paths.push(StoredPath(Path::new("game").join(path)));
                            excluded_paths.push(StoredPath(Path::new("game/images").join(path)));
                        }
                    }
                }
            }
        }
        let notes = if compiled && !scripts {
            vec!["No .rpy sources found; only fixed icon exclusions apply".into()]
        } else {
            vec![]
        };
        Ok(Some(EngineInfo {
            name: "Ren'Py".into(),
            version,
            excluded_paths,
            notes,
            version_check_overridden: self.allow_unknown_version,
        }))
    }
    fn classify(
        &self,
        root: &Path,
        relative: &Path,
        engine: &EngineInfo,
        profile: &Profile,
    ) -> io::Result<Action> {
        let copy = |reason: &str| Action::Copy {
            reason: reason.into(),
        };
        let skip = |reason: String| Action::Skip { reason };
        let source = root.join(relative);
        if fs::symlink_metadata(&source)?.file_type().is_symlink() {
            return Ok(match source.canonicalize() {
                Ok(target) if target.starts_with(root) => copy("symbolic link"),
                _ => skip("symlink points outside game or cannot be resolved".into()),
            });
        }
        let Ok(game_path) = relative.strip_prefix("game") else {
            return Ok(copy("outside game directory"));
        };
        let ext = relative
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if game_path.starts_with("saves")
            || game_path.starts_with("cache")
            || ["rpy", "rpyc", "rpym", "rpymc"].contains(&ext.as_str())
        {
            return Ok(copy("script, save, or cache"));
        }
        if ["rpa", "rpi"].contains(&ext.as_str()) {
            return Ok(copy("contents not processed"));
        }
        let name = game_path
            .file_name()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let at_root = game_path.parent() == Some(Path::new(""));
        let fixed = (at_root
            && (name.starts_with("presplash")
                || name == "icon.png"
                || name.starts_with("android-")
                || name.starts_with("ios-")))
            || game_path == Path::new("gui/window_icon.png");
        let normalized = relative.to_string_lossy().to_lowercase();
        let translated = game_path
            .strip_prefix("tl")
            .ok()
            .and_then(|p| {
                p.components()
                    .next()
                    .map(|_| p.components().skip(1).collect::<std::path::PathBuf>())
            })
            .map(|p| Path::new("game").join(p).to_string_lossy().to_lowercase());
        if fixed
            || engine.excluded_paths.iter().any(|p| {
                let icon = p.0.to_string_lossy().to_lowercase();
                normalized == icon || translated.as_ref().is_some_and(|path| path == &icon)
            })
        {
            return Ok(skip("loaded outside image pipeline".into()));
        }
        if ext == "webp" && !profile.reencode_webp {
            return Ok(skip("already WebP".into()));
        }
        if !["png", "jpg", "jpeg", "webp"].contains(&ext.as_str()) {
            return Ok(copy("extension not eligible"));
        }
        if !engine.version_check_overridden {
            match engine.version.as_deref() {
                None => return Ok(skip("engine version unknown".into())),
                Some(version) => {
                    let parts: Vec<u32> = version
                        .split('.')
                        .filter_map(|part| part.parse().ok())
                        .collect();
                    if parts.len() < 3 {
                        return Ok(skip("engine version unknown".into()));
                    }
                    if (parts[0], parts[1], parts[2]) < MIN_WEBP_VERSION {
                        return Ok(skip(format!(
                            "engine version {version} does not support WebP"
                        )));
                    }
                }
            }
        }
        Ok(match detect(&source)? {
            ImageFormat::AnimatedPng | ImageFormat::AnimatedWebp => skip("animated image".into()),
            ImageFormat::Webp if !profile.reencode_webp => skip("already WebP".into()),
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Webp => Action::Convert {
                kind: MediaKind::Image,
                target: "webp".into(),
                encoder: "cwebp".into(),
                settings: profile.clone(),
            },
            ImageFormat::Unsupported => skip("unsupported source format".into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reason(action: Action) -> String {
        match action {
            Action::Copy { reason } | Action::Skip { reason } => reason,
            _ => "convert".into(),
        }
    }
    #[test]
    fn classifies_fixture_and_icons_without_trusting_extensions() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let root = fixture.root().canonicalize()?;
        fs::create_dir_all(root.join("game/gui"))?;
        fs::copy(
            root.join("game/icon.png"),
            root.join("game/gui/window_icon.png"),
        )?;
        fs::copy(
            root.join("game/images/existing.webp"),
            root.join("game/images/webp.png"),
        )?;
        fs::copy(
            root.join("game/images/café.jpg"),
            root.join("game/images/jpeg.png"),
        )?;
        fs::write(root.join("game/images/old.tga"), b"other")?;
        fs::create_dir_all(root.join("game/saves"))?;
        fs::write(root.join("game/saves/save.png"), b"save")?;
        fs::create_dir_all(root.join("renpy/common"))?;
        fs::write(root.join("renpy/common/frame.png"), b"engine")?;
        fs::copy(root.join("game/icon.png"), root.join("game/custom.png"))?;
        fs::copy(
            root.join("game/icon.png"),
            root.join("game/images/Custom.PNG"),
        )?;
        fs::create_dir_all(root.join("game/tl/fr/images"))?;
        fs::copy(
            root.join("game/icon.png"),
            root.join("game/tl/fr/images/custom.png"),
        )?;
        fs::write(
            root.join("game/options.rpy"),
            "define config.window_icon = \"custom.png\"\n",
        )?;
        let adapter = RenpyAdapter::default();
        let engine = adapter.detect(&root)?.unwrap();
        let profile = Profile::default();
        let classify = |name: &str| {
            adapter
                .classify(&root, Path::new(name), &engine, &profile)
                .unwrap()
        };
        for name in [
            "game/images/opaque.png",
            "game/images/BG.PNG",
            "game/images/jpeg.png",
            "game/images/café.jpg",
        ] {
            assert_eq!(reason(classify(name)), "convert", "{name}");
        }
        for name in [
            "game/presplash.png",
            "game/icon.png",
            "game/gui/window_icon.png",
            "game/custom.png",
            "game/images/Custom.PNG",
            "game/tl/fr/images/custom.png",
        ] {
            assert_eq!(
                reason(classify(name)),
                "loaded outside image pipeline",
                "{name}"
            );
        }
        assert_eq!(
            reason(classify("game/images/animated.png")),
            "animated image"
        );
        assert_eq!(
            reason(classify("game/images/mislabelled.png")),
            "unsupported source format"
        );
        assert_eq!(reason(classify("game/images/webp.png")), "already WebP");
        assert_eq!(
            reason(classify("game/images/existing.webp")),
            "already WebP"
        );
        assert_eq!(
            reason(classify("game/archive.rpa")),
            "contents not processed"
        );
        assert_eq!(
            reason(classify("game/images/old.tga")),
            "extension not eligible"
        );
        assert_eq!(
            reason(classify("renpy/common/frame.png")),
            "outside game directory"
        );
        assert_eq!(
            reason(classify("game/script.rpyc")),
            "script, save, or cache"
        );
        assert_eq!(
            reason(classify("game/saves/save.png")),
            "script, save, or cache"
        );
        assert!(reason(classify("game/outside-link")).contains("outside"));
        let small = compressor_core::profile::load_profile(Some("small"))?;
        assert_eq!(
            reason(adapter.classify(&root, Path::new("game/images/webp.png"), &engine, &small)?),
            "convert"
        );
        assert_eq!(
            reason(adapter.classify(
                &root,
                Path::new("game/images/existing.webp"),
                &engine,
                &small
            )?),
            "convert"
        );
        Ok(())
    }
    #[test]
    fn gates_old_and_unknown_engines_and_honours_override() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let root = fixture.root();
        let path = Path::new("game/images/opaque.png");
        let adapter = RenpyAdapter::default();
        let mut engine = adapter.detect(root)?.unwrap();
        let profile = Profile::default();
        engine.version = Some("6.18.3".into());
        assert_eq!(
            reason(adapter.classify(root, path, &engine, &profile)?),
            "engine version 6.18.3 does not support WebP"
        );
        engine.version = None;
        assert_eq!(
            reason(adapter.classify(root, path, &engine, &profile)?),
            "engine version unknown"
        );
        engine.version_check_overridden = true;
        assert_eq!(
            reason(adapter.classify(root, path, &engine, &profile)?),
            "convert"
        );
        engine.version = Some("6.99.12".into());
        engine.version_check_overridden = false;
        assert_eq!(
            reason(adapter.classify(root, path, &engine, &profile)?),
            "convert"
        );
        Ok(())
    }
    #[test]
    fn compiled_only_notes_fixed_icon_fallback() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let root = fixture.root();
        fs::remove_file(root.join("game/options.rpy"))?;
        fs::remove_file(root.join("game/script.rpy"))?;
        let adapter = RenpyAdapter::default();
        let engine = adapter.detect(root)?.unwrap();
        assert!(engine.notes.iter().any(|s| s.contains("only fixed icon")));
        let output = tempfile::tempdir()?;
        let journal = compressor_core::journal::Journal::open(
            &output.path().join(".compressor/journal.jsonl"),
        )?;
        let plan = compressor_core::Plan {
            plan_version: 1,
            input_root: StoredPath(root.to_owned()),
            engine: engine.clone(),
            profile: Profile::default(),
            tools: vec![],
            entries: vec![],
        };
        compressor_core::journal::finish_cancelled(&plan, output.path(), &journal)?;
        let report: compressor_core::journal::Report =
            serde_json::from_slice(&fs::read(output.path().join(".compressor/report.json"))?)?;
        assert!(report
            .notes
            .iter()
            .any(|note| note.contains("only fixed icon")));
        assert_eq!(
            reason(adapter.classify(
                root,
                Path::new("game/icon.png"),
                &engine,
                &Profile::default()
            )?),
            "loaded outside image pipeline"
        );
        Ok(())
    }

    #[test]
    fn detects_versions_and_rejects_other_directories() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let adapter = RenpyAdapter::default();
        assert_eq!(
            adapter.detect(fixture.root())?.unwrap().version.as_deref(),
            Some("8.5.0")
        );
        fs::remove_file(fixture.root().join("renpy/vc_version.py"))?;
        fs::write(fixture.root().join("game/script_version.txt"), "(7, 4, 11)")?;
        assert_eq!(
            adapter.detect(fixture.root())?.unwrap().version.as_deref(),
            Some("7.4.11")
        );
        let unrelated = tempfile::tempdir()?;
        assert!(adapter.detect(unrelated.path())?.is_none());
        Ok(())
    }
}
