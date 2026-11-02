//! Ren'Py game detection and classification.
use compressor_core::{
    registry::EngineAdapter, Action, EngineInfo, MediaKind, OptionKind, OptionSpec,
    ResolvedSettings, StoredPath,
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
// Ren'Py 6.99.9 added Opus to default distributions:
// https://www.renpy.org/doc/html/changelog6.html#ren-py-6-99-9
pub const MIN_OPUS_VERSION: (u32, u32, u32) = (6, 99, 9);

fn supports_opus(engine: &EngineInfo) -> bool {
    if engine.version_check_overridden {
        return true;
    }
    let Some(parts) = engine.version.as_deref().and_then(|v| {
        let mut p = v.split('.').map(str::parse::<u32>);
        Some((p.next()?.ok()?, p.next()?.ok()?, p.next()?.ok()?))
    }) else {
        return false;
    };
    parts >= MIN_OPUS_VERSION
}

#[derive(Default)]
pub struct RenpyAdapter {
    pub allow_unknown_version: bool,
}
impl EngineAdapter for RenpyAdapter {
    fn id(&self) -> &'static str {
        "renpy"
    }
    fn options(&self) -> Vec<OptionSpec> {
        vec![OptionSpec {
            name: "ogg_target".into(),
            kind: OptionKind::Enum(vec!["opus".into(), "vorbis".into()]),
            default: serde_json::json!("opus"),
        }]
    }
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
        settings: &ResolvedSettings,
    ) -> io::Result<Action> {
        let image = settings.image();
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
        if ["ogg", "opus", "mp3", "flac"].contains(&ext.as_str()) {
            use compressor_core::{AudioCodec, AudioEncode, AudioProps, ConvertSettings};
            use compressor_media::audio::{self, Codec, Container};
            let info = match audio::sniff(&source) {
                Ok(info) => info,
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
                    ) =>
                {
                    return Ok(skip("unsupported audio format".into()))
                }
                Err(err) => return Err(err),
            };
            if info.channels > 2 {
                return Ok(skip("more than two channels".into()));
            }
            let mut target = if info.codec == Codec::Mp3 {
                AudioCodec::Mp3
            } else if ext == "opus" {
                AudioCodec::Opus
            } else if ext == "ogg" || (ext == "flac" && info.codec == Codec::Flac) {
                if settings
                    .engine_options
                    .get("ogg_target")
                    .and_then(|v| v.value.as_str())
                    .unwrap_or("opus")
                    == "vorbis"
                {
                    AudioCodec::Vorbis
                } else {
                    AudioCodec::Opus
                }
            } else {
                return Ok(skip("unsupported audio format".into()));
            };
            if target == AudioCodec::Opus && !supports_opus(engine) {
                if ext == "opus" {
                    return Ok(skip(format!(
                        "engine version {} does not support Opus",
                        engine.version.as_deref().unwrap_or("unknown")
                    )));
                }
                target = AudioCodec::Vorbis;
            }
            let audio = settings.audio();
            let nominal = match target {
                AudioCodec::Opus => audio.opus_kbps_per_channel,
                AudioCodec::Mp3 => audio.mp3_kbps_per_channel,
                AudioCodec::Vorbis => [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 250]
                    [usize::from(audio.vorbis_quality)],
            } * info.channels;
            if info.avg_kbps <= nominal {
                return Ok(skip("already at or below target bitrate".into()));
            }
            let container = match info.container {
                Container::Ogg => "ogg",
                Container::Mp3 => "mp3",
            };
            return Ok(Action::Convert {
                kind: MediaKind::Audio,
                target: container.into(),
                encoder: "ffmpeg".into(),
                envelope: None,
                settings: ConvertSettings::Audio(AudioEncode {
                    codec: target.clone(),
                    bitrate_kbps: if target == AudioCodec::Vorbis {
                        None
                    } else {
                        Some(nominal)
                    },
                    vorbis_quality: if target == AudioCodec::Vorbis {
                        Some(audio.vorbis_quality)
                    } else {
                        None
                    },
                    source: AudioProps {
                        channels: info.channels,
                        sample_rate: info.sample_rate,
                        duration_ms: info.duration_ms,
                    },
                }),
            });
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
        if ext == "webp" && !image.reencode_webp {
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
            ImageFormat::Webp if !image.reencode_webp => skip("already WebP".into()),
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Webp => Action::Convert {
                kind: MediaKind::Image,
                target: "webp".into(),
                encoder: "cwebp".into(),
                envelope: None,
                settings: compressor_core::ConvertSettings::Image(settings.image()),
            },
            ImageFormat::Unsupported => skip("unsupported source format".into()),
        })
    }
    fn plan_notes(
        &self,
        _root: &Path,
        engine: &EngineInfo,
        settings: &ResolvedSettings,
        entries: &[compressor_core::Entry],
    ) -> Vec<String> {
        if supports_opus(engine)
            || settings
                .engine_options
                .get("ogg_target")
                .and_then(|v| v.value.as_str())
                .unwrap_or("opus")
                != "opus"
        {
            return vec![];
        }
        // Every Vorbis conversion of a `.ogg` file here is a fallback from Opus.
        let fallback = entries.iter().any(|entry| {
            entry
                .path
                .0
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("ogg"))
                && matches!(
                    &entry.action,
                    Action::Convert {
                        settings: compressor_core::ConvertSettings::Audio(audio),
                        ..
                    } if audio.codec == compressor_core::AudioCodec::Vorbis
                )
        });
        if fallback {
            vec![format!("Opus was not used because engine version {} does not support Opus; Vorbis selected", engine.version.as_deref().unwrap_or("unknown"))]
        } else {
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn generate_audio(path: &Path, channels: u32, args: &[&str]) -> io::Result<()> {
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "anoisesrc=duration=5:sample_rate=44100",
                "-ac",
                &channels.to_string(),
            ])
            .args(args)
            .arg("-y")
            .arg(path)
            .status()?;
        assert!(status.success());
        Ok(())
    }
    #[test]
    fn classifies_audio_by_content_and_target() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let root = fixture.root();
        let dir = root.join("game/audio");
        fs::create_dir_all(&dir)?;
        generate_audio(
            &dir.join("THEME.OGG"),
            2,
            &["-c:a", "libvorbis", "-q:a", "8"],
        )?;
        generate_audio(
            &dir.join("mislabel.ogg"),
            2,
            &["-c:a", "libmp3lame", "-b:a", "192k", "-f", "mp3"],
        )?;
        generate_audio(
            &dir.join("quiet.ogg"),
            1,
            &["-c:a", "libvorbis", "-b:a", "32k"],
        )?;
        generate_audio(
            &dir.join("surround.ogg"),
            6,
            &["-c:a", "libvorbis", "-q:a", "5"],
        )?;
        generate_audio(&dir.join("flac.ogg"), 2, &["-c:a", "flac", "-f", "ogg"])?;
        generate_audio(&dir.join("native.flac"), 2, &["-c:a", "flac"])?;
        generate_audio(
            &dir.join("sound.mp3"),
            2,
            &["-c:a", "libmp3lame", "-b:a", "192k"],
        )?;
        fs::copy(dir.join("THEME.OGG"), dir.join("theme.opus"))?;
        fs::write(dir.join("click.wav"), b"wave")?;
        let adapter = RenpyAdapter::default();
        let engine = adapter.detect(root)?.unwrap();
        let specs = std::collections::BTreeMap::from([("renpy".into(), adapter.options())]);
        let settings = compressor_core::settings::resolve(None, &[], "renpy", &specs)?;
        let classify = |name: &str, settings: &ResolvedSettings| {
            adapter
                .classify(root, &Path::new("game/audio").join(name), &engine, settings)
                .unwrap()
        };
        match classify("THEME.OGG", &settings) {
            Action::Convert {
                settings: compressor_core::ConvertSettings::Audio(a),
                ..
            } => {
                assert_eq!(a.codec, compressor_core::AudioCodec::Opus);
                assert_eq!(a.bitrate_kbps, Some(96));
            }
            other => panic!("{other:?}"),
        }
        match classify("mislabel.ogg", &settings) {
            Action::Convert {
                settings: compressor_core::ConvertSettings::Audio(a),
                ..
            } => assert_eq!(a.codec, compressor_core::AudioCodec::Mp3),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            reason(classify("quiet.ogg", &settings)),
            "already at or below target bitrate"
        );
        assert_eq!(
            reason(classify("surround.ogg", &settings)),
            "more than two channels"
        );
        assert!(matches!(
            classify("flac.ogg", &settings),
            Action::Convert { .. }
        ));
        assert_eq!(
            reason(classify("native.flac", &settings)),
            "unsupported audio format"
        );
        assert!(matches!(
            classify("sound.mp3", &settings),
            Action::Convert {
                settings: compressor_core::ConvertSettings::Audio(compressor_core::AudioEncode {
                    codec: compressor_core::AudioCodec::Mp3,
                    ..
                }),
                ..
            }
        ));
        assert!(matches!(
            classify("theme.opus", &settings),
            Action::Convert {
                settings: compressor_core::ConvertSettings::Audio(compressor_core::AudioEncode {
                    codec: compressor_core::AudioCodec::Opus,
                    ..
                }),
                ..
            }
        ));
        assert_eq!(
            reason(classify("click.wav", &settings)),
            "extension not eligible"
        );
        let vorbis = compressor_core::settings::resolve(
            None,
            &[
                "engines.renpy.ogg_target=vorbis".into(),
                "audio.vorbis_quality=2".into(),
            ],
            "renpy",
            &specs,
        )?;
        assert!(matches!(
            classify("flac.ogg", &vorbis),
            Action::Convert {
                settings: compressor_core::ConvertSettings::Audio(compressor_core::AudioEncode {
                    codec: compressor_core::AudioCodec::Vorbis,
                    ..
                }),
                ..
            }
        ));
        match classify("THEME.OGG", &vorbis) {
            Action::Convert {
                settings: compressor_core::ConvertSettings::Audio(a),
                ..
            } => {
                assert_eq!(a.codec, compressor_core::AudioCodec::Vorbis);
                assert_eq!(a.vorbis_quality, Some(2));
            }
            other => panic!("{other:?}"),
        }
        Ok(())
    }
    #[test]
    fn opus_version_gate_falls_back_and_reports_it() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let root = fixture.root();
        let ogg = root.join("game/music.ogg");
        generate_audio(&ogg, 2, &["-c:a", "libvorbis", "-q:a", "8"])?;
        fs::copy(&ogg, root.join("game/music.opus"))?;
        let adapter = RenpyAdapter::default();
        let mut engine = adapter.detect(root)?.unwrap();
        let settings = compressor_core::settings::resolve(
            None,
            &[],
            "renpy",
            &std::collections::BTreeMap::from([("renpy".into(), adapter.options())]),
        )?;
        let mut registry = compressor_core::registry::TransformerRegistry::default();
        registry.register(
            "cwebp",
            std::sync::Arc::new(compressor_media::webp::WebpTransformer),
        );
        registry.register(
            "ffmpeg",
            std::sync::Arc::new(compressor_media::audio::AudioTransformer),
        );
        for version in [None, Some("6.99.8".to_owned())] {
            engine.version = version;
            let ogg_action =
                adapter.classify(root, Path::new("game/music.ogg"), &engine, &settings)?;
            assert!(matches!(
                ogg_action,
                Action::Convert {
                    settings: compressor_core::ConvertSettings::Audio(
                        compressor_core::AudioEncode {
                            codec: compressor_core::AudioCodec::Vorbis,
                            ..
                        }
                    ),
                    ..
                }
            ));
            assert!(reason(adapter.classify(
                root,
                Path::new("game/music.opus"),
                &engine,
                &settings
            )?)
            .contains("does not support Opus"));
            let plan = compressor_core::planning::build_plan(
                root,
                &adapter,
                engine.clone(),
                settings.clone(),
                &registry,
            )?;
            assert!(plan
                .engine
                .notes
                .iter()
                .any(|s| s.contains("Opus was not used")));
        }
        engine.version = None;
        engine.version_check_overridden = true;
        assert!(matches!(
            adapter.classify(root, Path::new("game/music.ogg"), &engine, &settings)?,
            Action::Convert {
                settings: compressor_core::ConvertSettings::Audio(compressor_core::AudioEncode {
                    codec: compressor_core::AudioCodec::Opus,
                    ..
                }),
                ..
            }
        ));
        let plan =
            compressor_core::planning::build_plan(root, &adapter, engine, settings, &registry)?;
        assert!(
            compressor_core::journal::build_report(&compressor_core::journal::make_manifest(
                plan,
                &[],
                false
            ))
            .notes
            .iter()
            .any(|s| s.contains("overridden"))
        );
        Ok(())
    }
    #[test]
    fn engine_image_override_reaches_planned_action() -> io::Result<()> {
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let adapter = RenpyAdapter::default();
        let engine = adapter.detect(fixture.root())?.unwrap();
        let dir = tempfile::tempdir()?;
        let profile = dir.path().join("profile.toml");
        fs::write(&profile, "[engines.renpy.image]\nquality = 78\n")?;
        let specs = std::collections::BTreeMap::from([("renpy".into(), adapter.options())]);
        let settings =
            compressor_core::settings::resolve(profile.to_str(), &[], adapter.id(), &specs)?;
        let action = adapter.classify(
            fixture.root(),
            Path::new("game/images/opaque.png"),
            &engine,
            &settings,
        )?;
        match action {
            Action::Convert {
                settings: compressor_core::ConvertSettings::Image(image),
                ..
            } => assert_eq!(image.quality, 78),
            _ => panic!("expected image conversion"),
        }
        assert_eq!(adapter.id(), "renpy");
        assert_eq!(settings.engine_options["ogg_target"].value, "opus");
        Ok(())
    }
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
        let profile = ResolvedSettings::default();
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
        let small = compressor_core::settings::resolve(
            Some("small"),
            &[],
            "renpy",
            &std::collections::BTreeMap::from([("renpy".into(), adapter.options())]),
        )?;
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
        let profile = ResolvedSettings::default();
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
            plan_version: 5,
            input_root: StoredPath(root.to_owned()),
            engine: engine.clone(),
            settings: ResolvedSettings::default(),
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
                &ResolvedSettings::default()
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
