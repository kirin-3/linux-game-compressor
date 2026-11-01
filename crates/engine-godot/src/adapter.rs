use compressor_core::{
    registry::EngineAdapter, Action, EngineInfo, OptionKind, OptionSpec, ResolvedSettings,
};
use std::{
    io,
    path::{Path, PathBuf},
};
#[derive(Default)]
pub struct GodotAdapter;
impl EngineAdapter for GodotAdapter {
    fn id(&self) -> &'static str {
        "godot"
    }
    fn options(&self) -> Vec<OptionSpec> {
        vec![OptionSpec {
            name: "min_texture_kib".into(),
            kind: OptionKind::Int { min: 0, max: 65536 },
            default: serde_json::json!(4),
        }]
    }
    fn detect(&self, root: &Path) -> io::Result<Option<EngineInfo>> {
        let mut files = std::fs::read_dir(root)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .map(|entry| entry.path())
            .collect::<Vec<PathBuf>>();
        files.sort();
        let executables: Vec<_> = files
            .iter()
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
            })
            .collect();
        let mut packs = Vec::new();
        for file in &files {
            let ext = file
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !["pck", "exe", "x86_64", ""].contains(&ext.as_str()) {
                continue;
            }
            if let Ok(Some((start, _))) = crate::locate_trailer(file) {
                if let Ok(header) = crate::read_header(file, start) {
                    packs.push((file, start, header));
                }
            }
        }
        if packs.is_empty() {
            return Ok(None);
        }
        let main = packs
            .iter()
            .find(|(_, start, _)| *start != 0)
            .or_else(|| {
                packs.iter().find(|(path, _, _)| {
                    executables
                        .iter()
                        .any(|exe| exe.file_stem() == path.file_stem())
                })
            })
            .unwrap_or(&packs[0]);
        let notes = packs
            .iter()
            .map(|(path, start, _)| {
                format!(
                    "pack: {} ({})",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    if *start == 0 {
                        "standalone"
                    } else {
                        "embedded"
                    }
                )
            })
            .collect();
        Ok(Some(EngineInfo {
            name: "Godot".into(),
            version: Some(format!(
                "{}.{}.{}",
                main.2.major, main.2.minor, main.2.patch
            )),
            excluded_paths: vec![],
            notes,
            version_check_overridden: false,
        }))
    }
    fn classify(
        &self,
        root: &Path,
        path: &Path,
        _: &EngineInfo,
        _: &ResolvedSettings,
    ) -> io::Result<Action> {
        let full = root.join(path);
        if std::fs::symlink_metadata(&full)?.file_type().is_symlink() {
            return Ok(Action::Skip {
                reason: "symbolic link".into(),
            });
        }
        let found = match crate::locate_trailer(&full) {
            Ok(v) => v,
            Err(_) => {
                return Ok(Action::Skip {
                    reason: "unreadable pack".into(),
                })
            }
        };
        let Some((start, len)) = found else {
            return Ok(Action::Copy {
                reason: "not a pack".into(),
            });
        };
        let skip = |reason: &str| Action::Skip {
            reason: reason.into(),
        };
        let header = match crate::read_header(&full, start) {
            Ok(h) => h,
            Err(_) => return Ok(skip("unreadable pack")),
        };
        // An embedded format-2 pack with an absolute file base cannot be moved out of its host.
        if !(2..=4).contains(&header.format)
            || header.major != 4
            || (start != 0 && header.format == 2 && header.flags & 2 == 0)
        {
            return Ok(skip("unsupported pack format"));
        }
        if header.flags & 1 != 0 {
            return Ok(skip("encrypted pack"));
        }
        if header.flags & 4 != 0 {
            return Ok(skip("sparse bundle pack"));
        }
        if start != 0 {
            if let Err(e) = crate::validate_embedded_host(&full, start) {
                let reason = e.to_string();
                return Ok(skip(if reason == "signed executable" {
                    "signed executable"
                } else {
                    "unsupported executable format"
                }));
            }
        }
        if crate::read_pack(&full, start, len).is_err() {
            return Ok(skip("unreadable pack"));
        }
        Ok(Action::Rebuild {
            format: "godot-pck".into(),
        })
    }
    fn expand(
        &self,
        root: &Path,
        path: &Path,
        engine: &EngineInfo,
        settings: &ResolvedSettings,
    ) -> io::Result<Option<Vec<compressor_core::registry::MemberPlan>>> {
        if !matches!(
            self.classify(root, path, engine, settings)?,
            Action::Rebuild { .. }
        ) {
            return Ok(None);
        }
        let full = root.join(path);
        let (start, len) =
            crate::locate_trailer(&full)?.ok_or_else(|| io::Error::other("pack missing"))?;
        let pack = crate::read_pack(&full, start, len)?;
        let mut out = Vec::with_capacity(pack.members.len());
        for member in &pack.members {
            let bytes = crate::read_member(&full, member)?;
            let action = if member.flags & 1 != 0 {
                Action::Copy {
                    reason: "encrypted member".into(),
                }
            } else {
                classify_member(&member.path, &bytes, settings)
            };
            out.push(compressor_core::registry::MemberPlan {
                member: member.path.clone(),
                size: member.size,
                hash: blake3::hash(&bytes).to_hex().to_string(),
                action,
            });
        }
        Ok(Some(out))
    }
}
fn classify_member(name: &str, bytes: &[u8], settings: &ResolvedSettings) -> Action {
    use compressor_core::{AudioCodec, AudioEncode, AudioProps, ConvertSettings, MediaKind};
    let skip = |reason: &str| Action::Skip {
        reason: reason.into(),
    };
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".ctex") {
        let texture = match crate::ctex::Texture::parse(bytes) {
            Ok(t) => t,
            Err(_) => return skip("unsupported texture type"),
        };
        if texture.mipmaps > 0 {
            return skip("mipmapped texture");
        }
        if texture.data_format == 0 || texture.data_format == 3 {
            return skip("VRAM-compressed texture");
        }
        let min = settings
            .engine_options
            .get("min_texture_kib")
            .and_then(|v| v.value.as_u64())
            .unwrap_or(4)
            * 1024;
        if (bytes.len() as u64) < min {
            return skip("below minimum texture size");
        }
        if texture.data_format == 2 {
            match texture.webp_kind() {
                Some("lossy") if !settings.image().reencode_webp => {
                    return skip("already lossy WebP")
                }
                Some("lossy" | "lossless") => {}
                _ => return skip("unsupported texture type"),
            }
        }
        return Action::Convert {
            kind: MediaKind::Image,
            target: "webp".into(),
            encoder: "godot-ctex".into(),
            settings: ConvertSettings::Image(settings.image()),
        };
    }
    if lower.ends_with(".sample") {
        return Action::Copy {
            reason: "unsupported audio resource".into(),
        };
    }
    if ![".oggvorbisstr", ".mp3str", ".res", ".tres"]
        .iter()
        .any(|x| lower.ends_with(x))
    {
        return Action::Copy {
            reason: "not a convertible resource".into(),
        };
    }
    let resource = match crate::resource::Resource::parse(bytes) {
        Ok(r) => r,
        Err(e) => {
            return Action::Copy {
                reason: if e.to_string() == "compressed resource" {
                    "compressed resource"
                } else {
                    "unsupported resource layout"
                }
                .into(),
            }
        }
    };
    let (codec, channels, rate, duration_ms, kbps) = if resource.class == "AudioStreamMP3" {
        let data = resource
            .bodies
            .iter()
            .enumerate()
            .find_map(|(i, _)| resource.property(i, "data"))
            .and_then(|v| crate::resource::parse_byte_array(v).ok());
        let Some(info) = data.and_then(|v| compressor_media::audio::sniff_bytes(v).ok()) else {
            return skip("unsupported audio resource");
        };
        if info.codec != compressor_media::audio::Codec::Mp3 {
            return skip("unsupported audio resource");
        }
        (
            AudioCodec::Mp3,
            info.channels,
            info.sample_rate,
            info.duration_ms,
            info.avg_kbps,
        )
    } else if resource.class == "AudioStreamOggVorbis" {
        let Ok(seq) = crate::vorbis::Sequence::from_resource(&resource) else {
            return skip("unsupported audio resource");
        };
        let header = &seq.pages[0][0];
        let channels = u32::from(header[11]);
        let duration = ((*seq.granules.last().unwrap_or(&0)).max(0) as u64) * 1000
            / u64::from(seq.sample_rate);
        let total: usize = seq.pages.iter().flatten().map(Vec::len).sum();
        let bitrate = (total as u64 * 8).checked_div(duration).unwrap_or(0) as u32;
        (
            AudioCodec::Vorbis,
            channels,
            seq.sample_rate,
            duration,
            bitrate,
        )
    } else {
        return Action::Copy {
            reason: if resource.class.starts_with("AudioStream") {
                "unsupported audio resource"
            } else {
                "not a convertible resource"
            }
            .into(),
        };
    };
    if channels > 2 {
        return skip("more than two channels");
    }
    if channels == 0 || duration_ms == 0 {
        return skip("unsupported audio resource");
    }
    let audio = settings.audio();
    let nominal = match codec {
        AudioCodec::Mp3 => audio.mp3_kbps_per_channel,
        AudioCodec::Vorbis => {
            [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 250][usize::from(audio.vorbis_quality)]
        }
        AudioCodec::Opus => unreachable!(),
    } * channels;
    if kbps <= nominal {
        return skip("already at or below target bitrate");
    }
    Action::Convert {
        kind: MediaKind::Audio,
        target: if codec == AudioCodec::Mp3 {
            "mp3"
        } else {
            "ogg"
        }
        .into(),
        encoder: "godot-audio".into(),
        settings: ConvertSettings::Audio(AudioEncode {
            codec: codec.clone(),
            bitrate_kbps: if codec == AudioCodec::Mp3 {
                Some(nominal)
            } else {
                None
            },
            vorbis_quality: if codec == AudioCodec::Vorbis {
                Some(audio.vorbis_quality)
            } else {
                None
            },
            source: AudioProps {
                channels,
                sample_rate: rate,
                duration_ms,
            },
        }),
    }
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    fn reason(action: Action) -> String {
        match action {
            Action::Skip { reason } | Action::Copy { reason } => reason,
            _ => "convert".into(),
        }
    }
    #[test]
    fn classifies_texture_and_misc_members() -> io::Result<()> {
        let settings = ResolvedSettings::default();
        let mut header = [0; 36];
        header[..4].copy_from_slice(b"GST2");
        let texture = |format, mipmaps, images: Vec<Vec<u8>>| {
            crate::ctex::Texture {
                header,
                data_format: format,
                width: 16,
                height: 16,
                mipmaps,
                image_format: 5,
                images,
            }
            .write()
            .unwrap()
        };
        let png = texture(1, 0, vec![vec![0; 5000]]);
        assert_eq!(
            reason(classify_member("x.ctex", &png, &settings)),
            "convert"
        );
        assert_eq!(
            reason(classify_member(
                "x.ctex",
                &texture(1, 0, vec![vec![0; 5]]),
                &settings
            )),
            "below minimum texture size"
        );
        assert_eq!(
            reason(classify_member(
                "x.ctex",
                &texture(1, 1, vec![vec![0; 5000], vec![0; 5]]),
                &settings
            )),
            "mipmapped texture"
        );
        let mut raw = png.clone();
        raw[36..40].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            reason(classify_member("x.ctex", &raw, &settings)),
            "VRAM-compressed texture"
        );
        let mut lossy = vec![0; 5000];
        lossy[..16].copy_from_slice(b"RIFFxxxxWEBPVP8 ");
        assert_eq!(
            reason(classify_member(
                "x.ctex",
                &texture(2, 0, vec![lossy]),
                &settings
            )),
            "already lossy WebP"
        );
        let mut lossless = vec![0; 5000];
        lossless[..16].copy_from_slice(b"RIFFxxxxWEBPVP8L");
        assert_eq!(
            reason(classify_member(
                "x.ctex",
                &texture(2, 0, vec![lossless]),
                &settings
            )),
            "convert"
        );
        let mut high = settings.clone();
        high.values.get_mut("image.reencode_webp").unwrap().value = serde_json::json!(true);
        let mut lossy = vec![0; 5000];
        lossy[..16].copy_from_slice(b"RIFFxxxxWEBPVP8 ");
        assert_eq!(
            reason(classify_member(
                "x.ctex",
                &texture(2, 0, vec![lossy]),
                &high
            )),
            "convert"
        );
        assert_eq!(
            reason(classify_member("x.ctex", b"broken", &settings)),
            "unsupported texture type"
        );
        assert_eq!(
            reason(classify_member("x.gdc", b"script", &settings)),
            "not a convertible resource"
        );
        assert_eq!(
            reason(classify_member("x.sample", b"data", &settings)),
            "unsupported audio resource"
        );
        assert_eq!(
            reason(classify_member("x.res", b"RSCC", &settings)),
            "compressed resource"
        );
        Ok(())
    }
    pub(crate) fn audio_resource(class: &str, audio: &[u8]) -> Vec<u8> {
        fn string(out: &mut Vec<u8>, s: &str) {
            out.extend(((s.len() + 1) as u32).to_le_bytes());
            out.extend(s.as_bytes());
            out.push(0);
        }
        let mut out = b"RSRC".to_vec();
        for n in [0u32, 0, 4, 0, 6] {
            out.extend(n.to_le_bytes());
        }
        string(&mut out, class);
        out.extend(0u64.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(0u64.to_le_bytes());
        out.extend([0; 44]);
        out.extend(1u32.to_le_bytes());
        string(&mut out, "data");
        out.extend(0u32.to_le_bytes());
        out.extend(1u32.to_le_bytes());
        string(&mut out, "local://main");
        let index = out.len();
        out.extend(0u64.to_le_bytes());
        let offset = out.len() as u64;
        out[index..index + 8].copy_from_slice(&offset.to_le_bytes());
        string(&mut out, class);
        out.extend(1u32.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(crate::resource::byte_array(audio).unwrap());
        out
    }
    #[test]
    fn mp3_planning_and_bitrate_skip() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("audio.mp3");
        let ok = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=2",
                "-ac",
                "2",
                "-c:a",
                "libmp3lame",
                "-b:a",
                "192k",
                "-y",
            ])
            .arg(&file)
            .status()?;
        assert!(ok.success());
        let resource = audio_resource("AudioStreamMP3", &std::fs::read(&file)?);
        let settings = ResolvedSettings::default();
        assert!(matches!(
            classify_member("sound.mp3str", &resource, &settings),
            Action::Convert {
                kind: compressor_core::MediaKind::Audio,
                ..
            }
        ));
        let mut high = settings.clone();
        high.values
            .get_mut("audio.mp3_kbps_per_channel")
            .unwrap()
            .value = serde_json::json!(160);
        assert_eq!(
            reason(classify_member("sound.mp3str", &resource, &high)),
            "already at or below target bitrate"
        );
        assert_eq!(
            reason(classify_member(
                "sound.res",
                &audio_resource("AudioStreamWAV", b"x"),
                &settings
            )),
            "unsupported audio resource"
        );
        Ok(())
    }
    #[test]
    fn vorbis_planning_channels_and_quality() -> io::Result<()> {
        let make = |channels| {
            let mut id = vec![0; 30];
            id[..7].copy_from_slice(b"\x01vorbis");
            id[11] = channels;
            id[12..16].copy_from_slice(&44100u32.to_le_bytes());
            crate::vorbis::Sequence {
                pages: vec![vec![id], vec![vec![42; 40000]]],
                granules: vec![0, 44100],
                sample_rate: 44100,
            }
        };
        let settings = ResolvedSettings::default();
        let stereo = crate::audio_transform::tests::vorbis_resource(&make(2))?;
        assert!(matches!(
            classify_member("sound.oggvorbisstr", &stereo, &settings),
            Action::Convert {
                kind: compressor_core::MediaKind::Audio,
                ..
            }
        ));
        let multichannel = crate::audio_transform::tests::vorbis_resource(&make(3))?;
        assert_eq!(
            reason(classify_member(
                "sound.oggvorbisstr",
                &multichannel,
                &settings
            )),
            "more than two channels"
        );
        Ok(())
    }
    #[test]
    fn pack_refusals_and_encrypted_member() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("game.pck");
        let engine = EngineInfo {
            name: "Godot".into(),
            version: None,
            excluded_paths: vec![],
            notes: vec![],
            version_check_overridden: false,
        };
        for (format, flags, expected) in [
            (4, 1, "encrypted pack"),
            (4, 4, "sparse bundle pack"),
            (5, 0, "unsupported pack format"),
        ] {
            std::fs::write(&path, compressor_test_fixtures::godot_pack(format, flags))?;
            assert_eq!(
                reason(GodotAdapter.classify(
                    dir.path(),
                    Path::new("game.pck"),
                    &engine,
                    &ResolvedSettings::default()
                )?),
                expected
            );
        }
        let mut pack = compressor_test_fixtures::godot_pack(4, 2);
        let n = pack.len();
        pack[n - 4..].copy_from_slice(&1u32.to_le_bytes());
        std::fs::write(&path, pack)?;
        let expanded = GodotAdapter
            .expand(
                dir.path(),
                Path::new("game.pck"),
                &engine,
                &ResolvedSettings::default(),
            )?
            .unwrap();
        assert_eq!(reason(expanded[1].action.clone()), "encrypted member");
        Ok(())
    }
    #[test]
    #[ignore = "requires example games"]
    fn plans_real_pack_members() -> io::Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../example_games/GODOT_1_Premature Ejaculator and a Succubus");
        let engine = GodotAdapter.detect(&root)?.unwrap();
        let members = GodotAdapter
            .expand(
                &root,
                Path::new("STST.pck"),
                &engine,
                &ResolvedSettings::default(),
            )?
            .unwrap();
        let converted = members
            .iter()
            .filter(|m| matches!(m.action, Action::Convert { .. }))
            .count();
        eprintln!("{} members, {converted} conversions", members.len());
        assert_eq!(members.len(), 1375);
        assert!(converted > 0);
        assert!(members.iter().any(|m| matches!(
            &m.action,
            Action::Convert {
                kind: compressor_core::MediaKind::Audio,
                ..
            }
        )));
        assert!(members.iter().any(|m| matches!(
            &m.action,
            Action::Convert {
                kind: compressor_core::MediaKind::Image,
                ..
            }
        )));
        Ok(())
    }
    #[test]
    fn detects_standalone_and_embedded_and_validates_option() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("STST.pck");
        std::fs::write(&path, compressor_test_fixtures::godot_pack(4, 2))?;
        std::fs::write(dir.path().join("STST.exe"), b"exe")?;
        let info = GodotAdapter.detect(dir.path())?.unwrap();
        assert_eq!(info.version.as_deref(), Some("4.7.0"));
        assert!(info.notes[0].contains("standalone"));
        std::fs::remove_file(path)?;
        let pack = compressor_test_fixtures::godot_pack(4, 2);
        let mut exe = vec![0; 64];
        exe.extend(&pack);
        exe.extend((pack.len() as u64).to_le_bytes());
        exe.extend(b"GDPC");
        std::fs::write(dir.path().join("STST.exe"), exe)?;
        assert!(GodotAdapter.detect(dir.path())?.unwrap().notes[0].contains("embedded"));
        let options = std::collections::BTreeMap::from([("godot".into(), GodotAdapter.options())]);
        assert!(compressor_core::settings::resolve(
            None,
            &["engines.godot.min_texture_kib=70000".into()],
            "godot",
            &options
        )
        .is_err());
        Ok(())
    }
}
