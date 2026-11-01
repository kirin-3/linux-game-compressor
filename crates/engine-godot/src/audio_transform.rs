use compressor_core::{
    registry::{Operation, Transformer},
    ConvertSettings, MediaKind, ToolSpec,
};
use std::{collections::BTreeMap, io, path::Path};
use tokio_util::sync::CancellationToken;
pub struct GodotAudioTransformer;
fn extract(resource: &crate::resource::Resource) -> io::Result<(usize, Vec<u8>)> {
    match resource.class.as_str() {
        "AudioStreamMP3" => {
            let (index, bytes) = resource
                .bodies
                .iter()
                .enumerate()
                .find_map(|(i, _)| resource.property(i, "data").map(|b| (i, b)))
                .ok_or_else(|| io::Error::other("unsupported audio resource"))?;
            Ok((index, crate::resource::parse_byte_array(bytes)?.to_vec()))
        }
        "AudioStreamOggVorbis" => {
            let index = resource
                .bodies
                .iter()
                .position(|b| b.class == "OggPacketSequence")
                .ok_or_else(|| io::Error::other("unsupported audio resource"))?;
            Ok((
                index,
                crate::vorbis::Sequence::from_resource(resource)?.to_ogg()?,
            ))
        }
        _ => Err(io::Error::other("unsupported audio resource")),
    }
}
fn allowed(class: &str, name: &str) -> bool {
    if class == "AudioStreamMP3" {
        name == "data"
    } else {
        ["packet_data", "granule_positions", "sampling_rate"].contains(&name)
    }
}
impl Transformer for GodotAudioTransformer {
    fn kind(&self) -> MediaKind {
        MediaKind::Audio
    }
    fn required_tools(&self) -> Vec<ToolSpec> {
        compressor_media::audio::AudioTransformer.required_tools()
    }
    fn required_tools_for(&self, settings: &ConvertSettings) -> Vec<ToolSpec> {
        compressor_media::audio::AudioTransformer.required_tools_for(settings)
    }
    fn transform<'a>(
        &'a self,
        src: &'a Path,
        dst: &'a Path,
        settings: &'a ConvertSettings,
        cancel: &'a CancellationToken,
    ) -> Operation<'a, Option<String>> {
        Box::pin(async move {
            self.transform_with_force(src, dst, settings, cancel, &CancellationToken::new())
                .await
        })
    }
    fn transform_with_force<'a>(
        &'a self,
        src: &'a Path,
        dst: &'a Path,
        settings: &'a ConvertSettings,
        cancel: &'a CancellationToken,
        force: &'a CancellationToken,
    ) -> Operation<'a, Option<String>> {
        Box::pin(async move {
            let resource = crate::resource::Resource::parse(&std::fs::read(src)?)?;
            let (index, audio) = extract(&resource)?;
            let input = tempfile::NamedTempFile::new()?;
            let output = tempfile::NamedTempFile::new()?;
            std::fs::write(input.path(), audio)?;
            let inner = compressor_media::audio::AudioTransformer;
            let note = inner
                .transform_with_force(input.path(), output.path(), settings, cancel, force)
                .await?;
            let bytes = std::fs::read(output.path())?;
            let mut changes = BTreeMap::new();
            if resource.class == "AudioStreamMP3" {
                changes.insert((index, "data".into()), crate::resource::byte_array(&bytes)?);
            } else {
                let seq = crate::vorbis::Sequence::from_ogg(&bytes)?;
                changes.insert(
                    (index, "packet_data".into()),
                    crate::resource::packet_arrays(&seq.pages)?,
                );
                changes.insert(
                    (index, "granule_positions".into()),
                    crate::resource::packed_int64(&seq.granules)?,
                );
                changes.insert(
                    (index, "sampling_rate".into()),
                    crate::resource::real(f64::from(seq.sample_rate)),
                );
            }
            std::fs::write(dst, resource.write(&changes)?)?;
            Ok(note)
        })
    }
    fn validate<'a>(
        &'a self,
        src: &'a Path,
        dst: &'a Path,
        settings: &'a ConvertSettings,
        cancel: &'a CancellationToken,
    ) -> Operation<'a, ()> {
        Box::pin(async move {
            self.validate_with_force(src, dst, settings, cancel, &CancellationToken::new())
                .await
        })
    }
    fn validate_with_force<'a>(
        &'a self,
        src: &'a Path,
        dst: &'a Path,
        settings: &'a ConvertSettings,
        cancel: &'a CancellationToken,
        force: &'a CancellationToken,
    ) -> Operation<'a, ()> {
        Box::pin(async move {
            let before = crate::resource::Resource::parse(&std::fs::read(src)?)
                .map_err(|_| io::Error::other("decode failed"))?;
            let after = crate::resource::Resource::parse(&std::fs::read(dst)?)
                .map_err(|_| io::Error::other("decode failed"))?;
            let (target, _) = extract(&before).map_err(|_| io::Error::other("decode failed"))?;
            if before.class != after.class || before.bodies.len() != after.bodies.len() {
                return Err(io::Error::other("resource properties changed"));
            }
            for (i, (a, b)) in before.bodies.iter().zip(&after.bodies).enumerate() {
                if a.class != b.class || a.properties.len() != b.properties.len() {
                    return Err(io::Error::other("resource properties changed"));
                }
                for (x, y) in a.properties.iter().zip(&b.properties) {
                    if x.name != y.name
                        || (!(i == target && allowed(&before.class, &x.name))
                            && before.property(i, &x.name) != after.property(i, &y.name))
                    {
                        return Err(io::Error::other("resource properties changed"));
                    }
                }
            }
            let (_, audio) = extract(&after).map_err(|_| io::Error::other("decode failed"))?;
            let file = tempfile::NamedTempFile::new()?;
            std::fs::write(file.path(), audio)?;
            compressor_media::audio::AudioTransformer::validate_output(
                file.path(),
                settings,
                cancel,
                force,
            )
            .await
        })
    }
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use compressor_core::{AudioCodec, AudioEncode, AudioProps};
    pub(crate) fn vorbis_resource(seq: &crate::vorbis::Sequence) -> io::Result<Vec<u8>> {
        fn string(out: &mut Vec<u8>, s: &str) {
            out.extend(((s.len() + 1) as u32).to_le_bytes());
            out.extend(s.as_bytes());
            out.push(0);
        }
        let names = [
            "packet_data",
            "granule_positions",
            "sampling_rate",
            "loop",
            "loop_offset",
            "bpm",
            "beat_count",
            "bar_beats",
        ];
        let mut out = b"RSRC".to_vec();
        for n in [0u32, 0, 4, 0, 6] {
            out.extend(n.to_le_bytes());
        }
        string(&mut out, "AudioStreamOggVorbis");
        out.extend(0u64.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(0u64.to_le_bytes());
        out.extend([0; 44]);
        out.extend((names.len() as u32).to_le_bytes());
        for n in names {
            string(&mut out, n);
        }
        out.extend(0u32.to_le_bytes());
        out.extend(2u32.to_le_bytes());
        string(&mut out, "sequence");
        let a = out.len();
        out.extend(0u64.to_le_bytes());
        string(&mut out, "main");
        let b = out.len();
        out.extend(0u64.to_le_bytes());
        let first = out.len() as u64;
        string(&mut out, "OggPacketSequence");
        out.extend(3u32.to_le_bytes());
        for (i, value) in [
            crate::resource::packet_arrays(&seq.pages)?,
            crate::resource::packed_int64(&seq.granules)?,
            crate::resource::real(seq.sample_rate as f64),
        ]
        .into_iter()
        .enumerate()
        {
            out.extend((i as u32).to_le_bytes());
            out.extend(value);
        }
        let second = out.len() as u64;
        string(&mut out, "AudioStreamOggVorbis");
        out.extend(5u32.to_le_bytes());
        for (i, value) in [(3, 1.0), (4, 2.5), (5, 120.0), (6, 4.0), (7, 4.0)] {
            out.extend((i as u32).to_le_bytes());
            out.extend(crate::resource::real(value));
        }
        out[a..a + 8].copy_from_slice(&first.to_le_bytes());
        out[b..b + 8].copy_from_slice(&second.to_le_bytes());
        Ok(out)
    }
    #[tokio::test]
    async fn converts_synthetic_vorbis_with_properties() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let ogg = dir.path().join("input.ogg");
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
                "libvorbis",
                "-q:a",
                "8",
                "-y",
            ])
            .arg(&ogg)
            .status()?;
        assert!(ok.success());
        let seq = crate::vorbis::Sequence::from_ogg(&std::fs::read(ogg)?)?;
        let original = vorbis_resource(&seq)?;
        let parsed = crate::resource::Resource::parse(&original)?;
        let settings = ConvertSettings::Audio(AudioEncode {
            codec: AudioCodec::Vorbis,
            bitrate_kbps: None,
            vorbis_quality: Some(3),
            source: AudioProps {
                channels: 2,
                sample_rate: seq.sample_rate,
                duration_ms: (*seq.granules.last().unwrap() as u64) * 1000
                    / u64::from(seq.sample_rate),
            },
        });
        let source = dir.path().join("in");
        let output = dir.path().join("out");
        std::fs::write(&source, original)?;
        let transform = GodotAudioTransformer;
        let cancel = CancellationToken::new();
        transform
            .transform(&source, &output, &settings, &cancel)
            .await?;
        transform
            .validate(&source, &output, &settings, &cancel)
            .await?;
        let result = crate::resource::Resource::parse(&std::fs::read(output)?)?;
        for name in ["loop", "loop_offset", "bpm", "beat_count", "bar_beats"] {
            assert_eq!(parsed.property(1, name), result.property(1, name));
        }
        Ok(())
    }
    #[tokio::test]
    #[ignore = "requires example game"]
    async fn converts_real_vorbis_resource() -> io::Result<()> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../example_games/GODOT_1_Premature Ejaculator and a Succubus/STST.pck");
        let (start, len) = crate::locate_trailer(&path)?.unwrap();
        let pack = crate::read_pack(&path, start, len)?;
        let member = pack
            .members
            .iter()
            .filter(|m| m.path.ends_with(".oggvorbisstr"))
            .min_by_key(|m| m.size)
            .unwrap();
        let bytes = crate::read_member(&path, member)?;
        let resource = crate::resource::Resource::parse(&bytes)?;
        let seq = crate::vorbis::Sequence::from_resource(&resource)?;
        let header = &seq.pages[0][0];
        let settings = ConvertSettings::Audio(AudioEncode {
            codec: AudioCodec::Vorbis,
            bitrate_kbps: None,
            vorbis_quality: Some(3),
            source: AudioProps {
                channels: header[11] as u32,
                sample_rate: seq.sample_rate,
                duration_ms: (*seq.granules.last().unwrap() as u64) * 1000
                    / u64::from(seq.sample_rate),
            },
        });
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("in");
        let output = dir.path().join("out");
        std::fs::write(&source, bytes)?;
        let cancel = CancellationToken::new();
        let transform = GodotAudioTransformer;
        transform
            .transform(&source, &output, &settings, &cancel)
            .await?;
        transform
            .validate(&source, &output, &settings, &cancel)
            .await?;
        Ok(())
    }
    #[tokio::test]
    async fn converts_mp3_resource_and_detects_corruption() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let audio = dir.path().join("input.mp3");
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
                "-b:a",
                "192k",
                "-c:a",
                "libmp3lame",
                "-y",
            ])
            .arg(&audio)
            .status()?;
        assert!(ok.success());
        let bytes = std::fs::read(&audio)?;
        let info = compressor_media::audio::sniff_bytes(&bytes)?;
        let resource = crate::adapter::tests::audio_resource("AudioStreamMP3", &bytes);
        let source = dir.path().join("source");
        let output = dir.path().join("output");
        std::fs::write(&source, resource)?;
        let settings = ConvertSettings::Audio(AudioEncode {
            codec: AudioCodec::Mp3,
            bitrate_kbps: Some(128),
            vorbis_quality: None,
            source: AudioProps {
                channels: info.channels,
                sample_rate: info.sample_rate,
                duration_ms: info.duration_ms,
            },
        });
        let transformer = GodotAudioTransformer;
        let cancel = CancellationToken::new();
        transformer
            .transform(&source, &output, &settings, &cancel)
            .await?;
        transformer
            .validate(&source, &output, &settings, &cancel)
            .await?;
        let parsed = crate::resource::Resource::parse(&std::fs::read(&output)?)?;
        let damaged = parsed.write(&BTreeMap::from([(
            (0, "data".into()),
            crate::resource::byte_array(b"bad")?,
        )]))?;
        std::fs::write(&output, damaged)?;
        assert_eq!(
            transformer
                .validate(&source, &output, &settings, &cancel)
                .await
                .unwrap_err()
                .to_string(),
            "decode failed"
        );
        Ok(())
    }
}
