//! RPG Maker MV/MZ adapter and asset envelope.
use compressor_core::registry::EnvelopeFormat;
use std::io;

pub const HEADER: [u8; 16] = *b"RPGMV\0\0\0\0\x03\x01\0\0\0\0\0";
pub struct RpgMakerEnvelope;
fn key(params: &serde_json::Value) -> io::Result<[u8; 16]> {
    let value = params["key"]
        .as_str()
        .ok_or_else(|| io::Error::other("missing encryption key"))?;
    if value.len() != 32 || !value.is_ascii() {
        return Err(io::Error::other("invalid encryption key"));
    }
    let mut key = [0; 16];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16)
            .map_err(|_| io::Error::other("invalid encryption key"))?;
    }
    Ok(key)
}
impl EnvelopeFormat for RpgMakerEnvelope {
    fn unwrap(&self, data: &[u8], params: &serde_json::Value) -> io::Result<Vec<u8>> {
        if data.len() < 32 || !data.starts_with(&HEADER) {
            return Err(io::Error::other(
                "unrecognised encrypted header or short payload",
            ));
        }
        let key = key(params)?;
        let mut payload = data[16..].to_vec();
        for (byte, k) in payload[..16].iter_mut().zip(key) {
            *byte ^= k;
        }
        Ok(payload)
    }
    fn wrap(&self, payload: &[u8], params: &serde_json::Value) -> io::Result<Vec<u8>> {
        if payload.len() < 16 {
            return Err(io::Error::other("payload shorter than 16 bytes"));
        }
        let key = key(params)?;
        let mut out = HEADER.to_vec();
        out.extend_from_slice(payload);
        for (byte, k) in out[16..32].iter_mut().zip(key) {
            *byte ^= k;
        }
        Ok(out)
    }
}
/// A seekable plaintext view into an RPG Maker encrypted asset.
pub struct DecryptReader<R> {
    inner: R,
    key: [u8; 16],
    pos: u64,
    len: u64,
}
impl<R: io::Read + io::Seek> DecryptReader<R> {
    pub fn new(mut inner: R, key: [u8; 16]) -> io::Result<Self> {
        let len = inner.seek(io::SeekFrom::End(0))?;
        if len < 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unrecognised encrypted header",
            ));
        }
        inner.seek(io::SeekFrom::Start(0))?;
        let mut header = [0; 16];
        inner.read_exact(&mut header)?;
        if header != HEADER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unrecognised encrypted header",
            ));
        }
        Ok(Self {
            inner,
            key,
            pos: 0,
            len: len - 16,
        })
    }
}
impl<R: io::Read + io::Seek> io::Read for DecryptReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.seek(io::SeekFrom::Start(
            self.pos
                .checked_add(16)
                .ok_or_else(|| io::Error::other("seek overflow"))?,
        ))?;
        let n = self.inner.read(buf)?;
        for (i, byte) in buf[..n].iter_mut().enumerate() {
            let at = self.pos + i as u64;
            if at < 16 {
                *byte ^= self.key[at as usize];
            }
        }
        self.pos += n as u64;
        Ok(n)
    }
}
impl<R: io::Read + io::Seek> io::Seek for DecryptReader<R> {
    fn seek(&mut self, from: io::SeekFrom) -> io::Result<u64> {
        let pos = match from {
            io::SeekFrom::Start(n) => i128::from(n),
            io::SeekFrom::Current(n) => i128::from(self.pos) + i128::from(n),
            io::SeekFrom::End(n) => i128::from(self.len) + i128::from(n),
        };
        self.pos = u64::try_from(pos)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid seek"))?;
        Ok(self.pos)
    }
}

trait ReadSeek: io::Read + io::Seek {}
impl<T: io::Read + io::Seek> ReadSeek for T {}

const PNG_PREFIX: [u8; 16] = *b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR";
#[derive(Clone)]
struct KeyInfo {
    value: Option<String>,
    source: &'static str,
}
#[derive(Default)]
pub struct RpgMakerAdapter {
    keys: std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, KeyInfo>>,
}
fn content_root(root: &std::path::Path) -> Option<(std::path::PathBuf, bool)> {
    for base in [root.to_owned(), root.join("www")] {
        if base.join("data/System.json").is_file() {
            if base.join("js/rmmz_core.js").is_file() {
                return Some((base, true));
            }
            if base.join("js/rpg_core.js").is_file() {
                return Some((base, false));
            }
        }
    }
    None
}
fn resolve_key(content: &std::path::Path) -> io::Result<KeyInfo> {
    let system: serde_json::Value =
        serde_json::from_slice(&std::fs::read(content.join("data/System.json"))?)
            .map_err(io::Error::other)?;
    let system_key = system["encryptionKey"]
        .as_str()
        .filter(|s| key(&serde_json::json!({"key": s})).is_ok())
        .map(str::to_owned);
    let mut images = Vec::new();
    let mut audio = Vec::new();
    for entry in walkdir::WalkDir::new(content)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        let ext = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if matches!(ext, "png_" | "rpgmvp") {
            images.push((entry.metadata()?.len(), entry.into_path()));
        } else if matches!(ext, "ogg_" | "rpgmvo") {
            audio.push(entry.into_path());
        }
    }
    images.sort();
    audio.sort();
    let prefix = |path: &std::path::Path| -> io::Result<Option<[u8; 16]>> {
        use std::io::Read;
        let mut data = [0; 32];
        if std::fs::File::open(path)?.read_exact(&mut data).is_err() || !data.starts_with(&HEADER) {
            return Ok(None);
        }
        Ok(Some(data[16..].try_into().unwrap()))
    };
    let first = images
        .first()
        .map(|(_, p)| prefix(p))
        .transpose()?
        .flatten();
    if let Some(first) = first {
        if let Some(candidate) = &system_key {
            let key = key(&serde_json::json!({"key":candidate}))?;
            if first
                .iter()
                .zip(key)
                .zip(PNG_PREFIX)
                .all(|((a, b), c)| a ^ b == c)
            {
                return Ok(KeyInfo {
                    value: Some(candidate.clone()),
                    source: "key from System.json",
                });
            }
        }
        let recovered: [u8; 16] = std::array::from_fn(|i| first[i] ^ PNG_PREFIX[i]);
        if let Some((_, second)) = images.get(1) {
            if !prefix(second)?.is_some_and(|data| {
                data.iter()
                    .zip(recovered)
                    .zip(PNG_PREFIX)
                    .all(|((a, b), c)| a ^ b == c)
            }) {
                return Ok(KeyInfo {
                    value: None,
                    source: "encryption key not found",
                });
            }
        }
        return Ok(KeyInfo {
            value: Some(recovered.iter().map(|b| format!("{b:02x}")).collect()),
            source: "key recovered from encrypted image",
        });
    }
    if let (Some(candidate), Some(path)) = (system_key, audio.first()) {
        if let Some(bytes) = prefix(path)? {
            let key = key(&serde_json::json!({"key":candidate}))?;
            if bytes[..4]
                .iter()
                .zip(key)
                .map(|(a, b)| a ^ b)
                .eq(b"OggS".iter().copied())
            {
                return Ok(KeyInfo {
                    value: Some(candidate),
                    source: "key from System.json",
                });
            }
        }
    }
    Ok(KeyInfo {
        value: None,
        source: "encryption key not found",
    })
}
impl compressor_core::registry::EngineAdapter for RpgMakerAdapter {
    fn id(&self) -> &'static str {
        "rpgmaker"
    }
    fn options(&self) -> Vec<compressor_core::OptionSpec> {
        vec![compressor_core::OptionSpec {
            name: "remove_unused".into(),
            kind: compressor_core::OptionKind::Bool,
            default: serde_json::json!(true),
        }]
    }
    fn detect(&self, root: &std::path::Path) -> io::Result<Option<compressor_core::EngineInfo>> {
        let root = root.canonicalize()?;
        let Some((content, mz)) = content_root(&root) else {
            if root.join("nw.dll").is_file()
                && root.join("locales").is_dir()
                && std::fs::read_dir(root)?.filter_map(Result::ok).any(|e| {
                    e.path()
                        .extension()
                        .is_some_and(|s| s.eq_ignore_ascii_case("exe"))
                })
            {
                return Err(io::Error::other(
                    "game appears packed with Enigma Virtual Box and is not supported",
                ));
            }
            return Ok(None);
        };
        let script = std::fs::read_to_string(content.join(if mz {
            "js/rmmz_core.js"
        } else {
            "js/rpg_core.js"
        }))?;
        let re = regex::Regex::new(r#"Utils\.RPGMAKER_VERSION\s*=\s*['\"]([^'\"]+)['\"]"#).unwrap();
        let version = re.captures(&script).map(|c| c[1].to_owned());
        let info = resolve_key(&content)?;
        self.keys
            .lock()
            .map_err(|_| io::Error::other("key cache poisoned"))?
            .insert(content, info);
        Ok(Some(compressor_core::EngineInfo {
            name: if mz { "RPG Maker MZ" } else { "RPG Maker MV" }.into(),
            version,
            excluded_paths: vec![],
            notes: vec![],
            version_check_overridden: false,
        }))
    }
    fn classify(
        &self,
        root: &std::path::Path,
        path: &std::path::Path,
        _engine: &compressor_core::EngineInfo,
        settings: &compressor_core::ResolvedSettings,
    ) -> io::Result<compressor_core::Action> {
        use compressor_core::{
            Action, AudioCodec, AudioEncode, AudioProps, ConvertSettings, Envelope, MediaKind,
            VideoEncode,
        };
        use compressor_media::{
            audio::{self, Codec},
            ImageFormat,
        };
        use std::io::{Read, Seek};
        let copy = |reason: &str| Action::Copy {
            reason: reason.into(),
        };
        let skip = |reason: &str| Action::Skip {
            reason: reason.into(),
        };
        let source = root.join(path);
        if std::fs::symlink_metadata(&source)?.file_type().is_symlink() {
            return Ok(copy("symbolic link"));
        }
        let Some((content, mz)) = content_root(root) else {
            return Ok(copy("engine runtime"));
        };
        let Ok(local) = source.strip_prefix(&content) else {
            return Ok(copy("engine runtime"));
        };
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let twin = match extension.as_str() {
            "m4a" => Some("ogg"),
            "rpgmvm" => Some("rpgmvo"),
            "m4a_" => Some("ogg_"),
            _ => None,
        }
        .is_some_and(|ext| source.with_extension(ext).is_file())
            || (mz
                && match extension.as_str() {
                    "rpgmvo" => Some("ogg_"),
                    "rpgmvm" => Some("m4a_"),
                    "rpgmvp" => Some("png_"),
                    _ => None,
                }
                .is_some_and(|ext| source.with_extension(ext).is_file()));
        if twin {
            if settings
                .engine_options
                .get("remove_unused")
                .and_then(|v| v.value.as_bool())
                .unwrap_or(true)
            {
                return Ok(Action::Remove {
                    reason: "unused on desktop".into(),
                });
            }
            return Ok(copy("unused removal disabled"));
        }
        let encrypted = matches!(
            extension.as_str(),
            "rpgmvp" | "png_" | "rpgmvo" | "ogg_" | "rpgmvm" | "m4a_"
        );
        let mut envelope = None;
        let mut reader: Box<dyn ReadSeek> = if encrypted {
            let mut file = std::fs::File::open(&source)?;
            let mut header = [0u8; 16];
            if file.read_exact(&mut header).is_err() || header != HEADER {
                return Ok(skip("unrecognised encrypted header"));
            }
            let cached_key = self
                .keys
                .lock()
                .map_err(|_| io::Error::other("key cache poisoned"))?
                .get(&content)
                .and_then(|v| v.value.clone());
            let Some(key_hex) = cached_key else {
                return Ok(skip("encryption key not found"));
            };
            let params = serde_json::json!({"key":key_hex});
            let reader = match DecryptReader::new(file, key(&params)?) {
                Ok(r) => r,
                Err(_) => return Ok(skip("unrecognised encrypted header")),
            };
            envelope = Some(Envelope {
                format: "rpgmaker".into(),
                params,
            });
            Box::new(reader)
        } else {
            Box::new(std::fs::File::open(&source)?)
        };
        if local.starts_with("icon") {
            return Ok(skip("window icon"));
        }
        if local.starts_with("effects") {
            return Ok(match compressor_media::detect_reader(&mut reader)? {
                ImageFormat::Unsupported => copy("other game file"),
                _ => skip("Effekseer texture"),
            });
        }
        if local.starts_with("img") {
            let format = compressor_media::detect_reader(&mut reader)?;
            return Ok(match format {
                ImageFormat::AnimatedPng | ImageFormat::AnimatedWebp => skip("animated image"),
                ImageFormat::Webp if !settings.image().reencode_webp => skip("already WebP"),
                ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Webp => {
                    let mut image = settings.image();
                    image.lossless = local.starts_with("img/parallaxes");
                    Action::Convert {
                        kind: MediaKind::Image,
                        target: "webp".into(),
                        encoder: "cwebp".into(),
                        settings: ConvertSettings::Image(image),
                        envelope,
                    }
                }
                ImageFormat::Unsupported => skip("unsupported source format"),
            });
        }
        if local.starts_with("audio") {
            reader.seek(io::SeekFrom::Start(0))?;
            let info = match audio::sniff_reader(&mut reader) {
                Ok(info) => info,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
                    ) =>
                {
                    return Ok(skip("unsupported audio format"))
                }
                Err(e) => return Err(e),
            };
            if info.channels > 2 {
                return Ok(skip("more than two channels"));
            }
            let audio = settings.audio();
            let codec = match info.codec {
                Codec::Vorbis | Codec::Flac => AudioCodec::Vorbis,
                Codec::Mp3 => AudioCodec::Mp3,
                _ => return Ok(skip("unsupported audio format")),
            };
            let nominal = match codec {
                AudioCodec::Vorbis => {
                    [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 250][audio.vorbis_quality as usize]
                }
                AudioCodec::Mp3 => audio.mp3_kbps_per_channel,
                _ => unreachable!(),
            } * info.channels;
            if info.avg_kbps <= nominal {
                return Ok(skip("already at or below target bitrate"));
            }
            return Ok(Action::Convert {
                kind: MediaKind::Audio,
                target: if codec == AudioCodec::Mp3 {
                    "mp3"
                } else {
                    "ogg"
                }
                .into(),
                encoder: "ffmpeg".into(),
                settings: ConvertSettings::Audio(AudioEncode {
                    codec: codec.clone(),
                    bitrate_kbps: (codec == AudioCodec::Mp3).then_some(nominal),
                    vorbis_quality: (codec == AudioCodec::Vorbis).then_some(audio.vorbis_quality),
                    source: AudioProps {
                        channels: info.channels,
                        sample_rate: info.sample_rate,
                        duration_ms: info.duration_ms,
                    },
                }),
                envelope,
            });
        }
        if local.starts_with("movies") || local.starts_with("videos") {
            if !compressor_media::video::has_video_magic(&source)? {
                return Ok(copy("other game file"));
            }
            let props = match compressor_media::video::probe(&source) {
                Ok(p) => p,
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    return Ok(skip(&e.to_string()))
                }
                Err(e) => return Err(e),
            };
            let video = settings.video();
            let opus_kbps = props
                .audio_channels
                .map(|n| n * video.opus_kbps_per_channel);
            return Ok(Action::Convert {
                kind: MediaKind::Video,
                target: "webm".into(),
                encoder: "ffmpeg-video".into(),
                settings: ConvertSettings::Video(VideoEncode {
                    crf: video.crf,
                    cpu_used: 5 - video.effort,
                    opus_kbps,
                    source: props,
                }),
                envelope: None,
            });
        }
        Ok(copy("other game file"))
    }
    fn plan_notes(
        &self,
        root: &std::path::Path,
        _engine: &compressor_core::EngineInfo,
        _settings: &compressor_core::ResolvedSettings,
        _entries: &[compressor_core::Entry],
    ) -> Vec<String> {
        content_root(root)
            .and_then(|(p, _)| {
                self.keys
                    .lock()
                    .ok()?
                    .get(&p)
                    .map(|info| info.source.to_owned())
            })
            .into_iter()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires example games"]
    fn unwraps_assets_in_all_example_games() -> io::Result<()> {
        let games = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../example_games");
        let mut tested = 0;
        for entry in std::fs::read_dir(games)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !["MV_1_", "MV_2_", "MZ_1_", "MZ_2_"]
                .iter()
                .any(|p| name.starts_with(p))
            {
                continue;
            }
            let paths: Vec<_> = walkdir::WalkDir::new(entry.path())
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_file())
                .map(|e| e.into_path())
                .filter(|p| {
                    matches!(
                        p.extension().and_then(|s| s.to_str()),
                        Some("rpgmvp" | "png_" | "rpgmvo" | "ogg_")
                    )
                })
                .collect();
            let image = paths
                .iter()
                .find(|p| {
                    matches!(
                        p.extension().and_then(|s| s.to_str()),
                        Some("rpgmvp" | "png_")
                    )
                })
                .unwrap();
            let encrypted = std::fs::read(image)?;
            let prefix = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR";
            let key: String = encrypted[16..32]
                .iter()
                .zip(prefix)
                .map(|(a, b)| format!("{:02x}", a ^ b))
                .collect();
            let params = serde_json::json!({"key": key});
            for path in paths {
                let data = RpgMakerEnvelope.unwrap(&std::fs::read(&path)?, &params)?;
                assert!(
                    data.starts_with(b"\x89PNG\r\n\x1a\n") || data.starts_with(b"OggS"),
                    "{}",
                    path.display()
                );
            }
            tested += 1;
        }
        assert_eq!(tested, 4);
        Ok(())
    }
    #[test]
    fn decrypt_reader_seeks_across_xor_boundary() -> io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let payload: Vec<u8> = (0..80).collect();
        let encrypted = compressor_test_fixtures::RpgMakerFixture::encrypt(&payload);
        let mut reader = DecryptReader::new(
            io::Cursor::new(encrypted),
            compressor_test_fixtures::RpgMakerFixture::KEY,
        )?;
        let mut part = [0; 12];
        reader.seek(SeekFrom::Start(10))?;
        reader.read_exact(&mut part)?;
        assert_eq!(&part, &payload[10..22]);
        reader.seek(SeekFrom::End(-5))?;
        let mut tail = Vec::new();
        reader.read_to_end(&mut tail)?;
        assert_eq!(tail, payload[75..]);
        reader.seek(SeekFrom::Start(0))?;
        let mut all = Vec::new();
        reader.read_to_end(&mut all)?;
        assert_eq!(all, payload);
        assert!(reader.seek(SeekFrom::Start(0)).is_ok());
        assert!(reader.seek(SeekFrom::Current(-1)).is_err());
        Ok(())
    }
    #[test]
    fn classifies_rpgmaker_assets_from_decrypted_readers() -> io::Result<()> {
        use compressor_core::{registry::EngineAdapter, Action, AudioCodec, ConvertSettings};
        let fixture = compressor_test_fixtures::RpgMakerFixture::new(true, true)?;
        let root = fixture.root();
        let adapter = RpgMakerAdapter::default();
        let info = adapter.detect(root)?.unwrap();
        let settings = compressor_core::ResolvedSettings::default();
        let classify = |path: &str| {
            adapter
                .classify(root, std::path::Path::new(path), &info, &settings)
                .unwrap()
        };
        assert!(
            matches!(classify("img/pictures/sample.png_"), Action::Convert { settings: ConvertSettings::Image(ref image), envelope: Some(_), .. } if !image.lossless)
        );
        assert!(
            matches!(classify("img/parallaxes/sample.png_"), Action::Convert { settings: ConvertSettings::Image(ref image), envelope: Some(_), .. } if image.lossless)
        );
        assert!(
            matches!(classify("audio/bgm/sample.ogg_"), Action::Convert { settings: ConvertSettings::Audio(ref audio), envelope: Some(_), .. } if audio.codec == AudioCodec::Vorbis)
        );
        assert!(
            matches!(classify("audio/bgm/sample.m4a_"), Action::Remove { ref reason } if reason == "unused on desktop")
        );
        let renpy_fixture = compressor_test_fixtures::Fixture::new()?;
        let animated = std::fs::read(renpy_fixture.root().join("game/images/animated.png"))?;
        std::fs::write(
            root.join("img/pictures/animated.png_"),
            compressor_test_fixtures::RpgMakerFixture::encrypt(&animated),
        )?;
        assert!(
            matches!(classify("img/pictures/animated.png_"), Action::Skip { ref reason } if reason == "animated image")
        );
        assert!(matches!(classify("data/System.json"), Action::Copy { .. }));
        std::fs::write(root.join("img/pictures/bad.png_"), b"not a valid header")?;
        assert!(
            matches!(classify("img/pictures/bad.png_"), Action::Skip { ref reason } if reason == "unrecognised encrypted header")
        );
        std::fs::create_dir(root.join("icon"))?;
        std::fs::write(root.join("icon/icon.png"), b"icon")?;
        assert!(
            matches!(classify("icon/icon.png"), Action::Skip { ref reason } if reason == "window icon")
        );
        std::fs::create_dir(root.join("effects"))?;
        std::fs::write(root.join("effects/texture.png"), &animated)?;
        assert!(
            matches!(classify("effects/texture.png"), Action::Skip { ref reason } if reason == "Effekseer texture")
        );
        std::fs::write(root.join("effects/burst.efkefc"), b"SKFE")?;
        assert!(
            matches!(classify("effects/burst.efkefc"), Action::Copy { ref reason } if reason == "other game file")
        );
        let movie = root.join("movies/disguised.dat");
        assert!(std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x48:rate=10:duration=1",
                "-c:v",
                "libvpx-vp9",
                "-f",
                "webm",
                "-y"
            ])
            .arg(&movie)
            .status()?
            .success());
        assert!(matches!(
            classify("movies/disguised.dat"),
            Action::Convert {
                settings: ConvertSettings::Video(_),
                ..
            }
        ));
        Ok(())
    }
    #[test]
    fn removes_matching_twins_with_engine_specific_rules() -> io::Result<()> {
        use compressor_core::{registry::EngineAdapter, Action};
        for mz in [false, true] {
            let fixture = compressor_test_fixtures::RpgMakerFixture::new(mz, true)?;
            let adapter = RpgMakerAdapter::default();
            let info = adapter.detect(fixture.root())?.unwrap();
            let options =
                std::collections::BTreeMap::from([("rpgmaker".into(), adapter.options())]);
            let on = compressor_core::settings::resolve(None, &[], "rpgmaker", &options)?;
            let off = compressor_core::settings::resolve(
                None,
                &["engines.rpgmaker.remove_unused=false".into()],
                "rpgmaker",
                &options,
            )?;
            let twin = if mz {
                "audio/bgm/sample.m4a_"
            } else {
                "www/audio/bgm/sample.rpgmvm"
            };
            assert!(matches!(
                adapter.classify(fixture.root(), twin.as_ref(), &info, &on)?,
                Action::Remove { .. }
            ));
            assert!(matches!(
                adapter.classify(fixture.root(), twin.as_ref(), &info, &off)?,
                Action::Copy { .. }
            ));
            let base = fixture.content().join("audio/bgm");
            std::fs::write(base.join("orphan.m4a"), b"not audio")?;
            let orphan = if mz {
                "audio/bgm/orphan.m4a"
            } else {
                "www/audio/bgm/orphan.m4a"
            };
            assert!(matches!(
                adapter.classify(fixture.root(), orphan.as_ref(), &info, &on)?,
                Action::Skip { .. }
            ));
            std::fs::copy(
                base.join(if mz { "sample.ogg_" } else { "sample.rpgmvo" }),
                base.join("old.rpgmvo"),
            )?;
            std::fs::copy(
                base.join(if mz { "sample.ogg_" } else { "sample.rpgmvo" }),
                base.join("old.ogg_"),
            )?;
            let old = if mz {
                "audio/bgm/old.rpgmvo"
            } else {
                "www/audio/bgm/old.rpgmvo"
            };
            assert_eq!(
                matches!(
                    adapter.classify(fixture.root(), old.as_ref(), &info, &on)?,
                    Action::Remove { .. }
                ),
                mz
            );
        }
        Ok(())
    }
    #[test]
    fn detects_games_and_resolves_keys() -> io::Result<()> {
        use compressor_core::registry::EngineAdapter;
        for mz in [false, true] {
            for with_key in [false, true] {
                let fixture = compressor_test_fixtures::RpgMakerFixture::new(mz, with_key)?;
                let adapter = RpgMakerAdapter::default();
                let info = adapter.detect(fixture.root())?.unwrap();
                assert_eq!(info.name, if mz { "RPG Maker MZ" } else { "RPG Maker MV" });
                assert_eq!(
                    info.version.as_deref(),
                    Some(if mz { "1.8.0" } else { "1.6.2" })
                );
                let key = adapter
                    .keys
                    .lock()
                    .unwrap()
                    .get(fixture.content())
                    .unwrap()
                    .clone();
                assert_eq!(
                    key.value.as_deref(),
                    Some("00112233445566778899aabbccddeeff")
                );
                assert_eq!(
                    key.source,
                    if with_key {
                        "key from System.json"
                    } else {
                        "key recovered from encrypted image"
                    }
                );
            }
        }
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("Game.exe"), b"")?;
        std::fs::write(dir.path().join("nw.dll"), b"")?;
        std::fs::create_dir(dir.path().join("locales"))?;
        assert!(RpgMakerAdapter::default()
            .detect(dir.path())
            .unwrap_err()
            .to_string()
            .contains("Enigma Virtual Box"));
        let audio_only = compressor_test_fixtures::RpgMakerFixture::new(false, true)?;
        std::fs::remove_dir_all(audio_only.content().join("img"))?;
        assert_eq!(
            resolve_key(audio_only.content())?.source,
            "key from System.json"
        );
        let wrong = compressor_test_fixtures::RpgMakerFixture::new(true, true)?;
        std::fs::write(
            wrong.content().join("data/System.json"),
            br#"{"encryptionKey":"ffffffffffffffffffffffffffffffff"}"#,
        )?;
        assert_eq!(
            resolve_key(wrong.content())?.source,
            "key recovered from encrypted image"
        );
        let fixture = compressor_test_fixtures::RpgMakerFixture::new(false, false)?;
        std::fs::remove_dir_all(fixture.content().join("img"))?;
        let adapter = RpgMakerAdapter::default();
        adapter.detect(fixture.root())?;
        assert!(adapter
            .keys
            .lock()
            .unwrap()
            .get(fixture.content())
            .unwrap()
            .value
            .is_none());
        Ok(())
    }
    #[test]
    fn envelope_roundtrip_and_rejects_invalid_data() -> io::Result<()> {
        let params = serde_json::json!({"key": "00112233445566778899aabbccddeeff"});
        let payload = b"a payload of at least sixteen bytes";
        let wrapped = RpgMakerEnvelope.wrap(payload, &params)?;
        assert_eq!(RpgMakerEnvelope.unwrap(&wrapped, &params)?, payload);
        assert!(RpgMakerEnvelope
            .unwrap(b"incorrect header and long enough content", &params)
            .is_err());
        assert!(RpgMakerEnvelope.wrap(b"short", &params).is_err());
        Ok(())
    }
}
