//! Content-based video inspection.
use compressor_core::VideoProps;
use std::{
    fs::File,
    io::{self, Read},
    path::Path,
    process::Command,
};

fn unsupported(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason.to_owned())
}

pub fn has_video_magic(path: &Path) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let mut header = [0u8; 65536];
    let n = file.read(&mut header)?;
    let h = &header[..n];
    Ok(h.starts_with(b"\x1a\x45\xdf\xa3")
        || (h.len() >= 8 && &h[4..8] == b"ftyp")
        || (h.len() >= 12 && &h[..4] == b"RIFF" && &h[8..12] == b"AVI ")
        || (h.starts_with(b"OggS") && h.windows(7).any(|w| w == b"\x80theora"))
        || h.starts_with(b"\x00\x00\x01\xba"))
}

pub fn probe(path: &Path) -> io::Result<VideoProps> {
    if !has_video_magic(path)? {
        return Err(unsupported("unsupported video format"));
    }
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
        ])
        .arg(path)
        .output()?;
    if !output.status.success() {
        return Err(unsupported("unsupported video format"));
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| unsupported("unsupported video format"))?;
    let streams = json["streams"]
        .as_array()
        .ok_or_else(|| unsupported("unsupported video format"))?;
    let videos: Vec<_> = streams
        .iter()
        .filter(|s| s["codec_type"] == "video")
        .collect();
    if videos.len() != 1 {
        return Err(unsupported("unsupported video format"));
    }
    let audio: Vec<_> = streams
        .iter()
        .filter(|s| s["codec_type"] == "audio")
        .collect();
    if audio.len() > 1
        || audio
            .first()
            .is_some_and(|s| s["channels"].as_u64().is_none_or(|n| n == 0 || n > 2))
    {
        return Err(unsupported("unsupported audio track"));
    }
    let video = videos[0];
    let width = video["width"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| unsupported("unsupported video format"))?;
    let height = video["height"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| unsupported("unsupported video format"))?;
    let fps = video["avg_frame_rate"]
        .as_str()
        .filter(|s| *s != "0/0")
        .or_else(|| video["r_frame_rate"].as_str())
        .filter(|s| *s != "0/0")
        .ok_or_else(|| unsupported("unsupported video format"))?
        .to_owned();
    let alpha = matches!(video["codec_name"].as_str(), Some("vp8" | "vp9"))
        && video["tags"]["alpha_mode"].as_str() == Some("1");
    let container_duration = json["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0);
    let packets = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=duration_time",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()?;
    let durations: Vec<f64> = String::from_utf8_lossy(&packets.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n > 0.0)
        .collect();
    let total: f64 = durations.iter().sum();
    let duration_ms = match container_duration {
        Some(duration)
            if packets.status.success() && !durations.is_empty() && duration <= total + 1.0 =>
        {
            (duration * 1000.0).round() as u64
        }
        _ => decode_duration(path, alpha, video["codec_name"].as_str() == Some("vp8"))
            .map_err(|_| unsupported("unsupported video format"))?,
    };
    Ok(VideoProps {
        width,
        height,
        fps,
        duration_ms,
        alpha,
        audio_channels: audio
            .first()
            .and_then(|s| s["channels"].as_u64())
            .map(|n| n as u32),
    })
}

fn decode_duration(path: &Path, alpha: bool, vp8: bool) -> io::Result<u64> {
    let mut command = Command::new("ffmpeg");
    command.args(["-nostdin", "-v", "error", "-xerror"]);
    if alpha {
        command.args(["-c:v", if vp8 { "libvpx" } else { "libvpx-vp9" }]);
    }
    let decoded = command
        .arg("-i")
        .arg(path)
        .args(["-map", "0:v:0", "-f", "null", "-", "-progress", "pipe:1"])
        .output()?;
    if !decoded.status.success() {
        return Err(io::Error::other("decode failed"));
    }
    let duration_us = String::from_utf8_lossy(&decoded.stdout)
        .lines()
        .filter_map(|l| {
            l.strip_prefix("out_time_us=")
                .and_then(|n| n.parse::<u64>().ok())
        })
        .next_back()
        .filter(|n| *n > 0)
        .ok_or_else(|| io::Error::other("decode failed"))?;
    Ok((duration_us + 500) / 1000)
}

pub struct VideoTransformer;
impl compressor_core::registry::Transformer for VideoTransformer {
    fn kind(&self) -> compressor_core::MediaKind {
        compressor_core::MediaKind::Video
    }
    fn required_tools(&self) -> Vec<compressor_core::ToolSpec> {
        vec![
            compressor_core::ToolSpec {
                name: "ffmpeg".into(),
                package: "ffmpeg".into(),
                features: vec!["libvpx-vp9".into(), "libopus".into()],
            },
            compressor_core::ToolSpec {
                name: "ffprobe".into(),
                package: "ffmpeg".into(),
                features: vec![],
            },
        ]
    }
    fn required_tools_for(
        &self,
        settings: &compressor_core::ConvertSettings,
    ) -> Vec<compressor_core::ToolSpec> {
        let mut tools = self.required_tools();
        if matches!(settings, compressor_core::ConvertSettings::Video(v) if v.source.alpha) {
            tools[0]
                .features
                .extend(["decoder:libvpx".into(), "decoder:libvpx-vp9".into()]);
        }
        tools
    }
    fn transform<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        settings: &'a compressor_core::ConvertSettings,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> compressor_core::registry::Operation<'a, Option<String>> {
        Box::pin(async move {
            self.transform_with_force(
                source,
                output,
                settings,
                cancel,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
        })
    }
    fn transform_with_force<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        settings: &'a compressor_core::ConvertSettings,
        cancel: &'a tokio_util::sync::CancellationToken,
        force: &'a tokio_util::sync::CancellationToken,
    ) -> compressor_core::registry::Operation<'a, Option<String>> {
        Box::pin(async move {
            let compressor_core::ConvertSettings::Video(video) = settings else {
                return Err(io::Error::other("expected video settings"));
            };
            let mut args: Vec<std::ffi::OsString> = ["-nostdin", "-v", "error"]
                .into_iter()
                .map(Into::into)
                .collect();
            if video.source.alpha {
                let source_path = source.to_owned();
                let probe = tokio::task::spawn_blocking(move || video_codec(&source_path))
                    .await
                    .map_err(io::Error::other)??;
                args.extend([
                    "-c:v".into(),
                    if probe == "vp8" {
                        "libvpx".into()
                    } else {
                        "libvpx-vp9".into()
                    },
                ]);
            }
            args.extend(["-i".into(), source.as_os_str().to_owned()]);
            args.extend(
                [
                    "-map",
                    "0:v:0",
                    "-map",
                    "0:a:0?",
                    "-map_metadata",
                    "-1",
                    "-fps_mode",
                    "passthrough",
                    "-c:v",
                    "libvpx-vp9",
                    "-crf",
                ]
                .into_iter()
                .map(Into::into),
            );
            args.push(video.crf.to_string().into());
            args.extend(
                ["-b:v", "0", "-deadline", "good", "-cpu-used"]
                    .into_iter()
                    .map(Into::into),
            );
            args.push(video.cpu_used.to_string().into());
            args.extend(
                ["-row-mt", "1", "-g", "240", "-pix_fmt"]
                    .into_iter()
                    .map(Into::into),
            );
            args.push(if video.source.alpha {
                "yuva420p".into()
            } else {
                "yuv420p".into()
            });
            if video.source.alpha {
                args.extend(
                    ["-auto-alt-ref", "0", "-lag-in-frames", "0"]
                        .into_iter()
                        .map(Into::into),
                );
            }
            if let Some(kbps) = video.opus_kbps {
                args.extend([
                    "-c:a".into(),
                    "libopus".into(),
                    "-b:a".into(),
                    format!("{kbps}k").into(),
                ]);
            }
            args.extend([
                "-f".into(),
                "webm".into(),
                "-y".into(),
                output.as_os_str().to_owned(),
            ]);
            let result =
                compressor_core::process::run_process("ffmpeg".as_ref(), args, cancel, force)
                    .await?;
            if result.cancelled {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            if !result.status.success() {
                return Err(io::Error::other(format!(
                    "ffmpeg: {}",
                    String::from_utf8_lossy(&result.stderr)
                )));
            }
            Ok(None)
        })
    }
    fn validate<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        settings: &'a compressor_core::ConvertSettings,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> compressor_core::registry::Operation<'a, ()> {
        Box::pin(async move {
            self.validate_with_force(
                source,
                output,
                settings,
                cancel,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
        })
    }
    fn validate_with_force<'a>(
        &'a self,
        _source: &'a Path,
        output: &'a Path,
        settings: &'a compressor_core::ConvertSettings,
        cancel: &'a tokio_util::sync::CancellationToken,
        _force: &'a tokio_util::sync::CancellationToken,
    ) -> compressor_core::registry::Operation<'a, ()> {
        Box::pin(async move {
            let compressor_core::ConvertSettings::Video(video) = settings else {
                return Err(io::Error::other("expected video settings"));
            };
            if cancel.is_cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let path = output.to_owned();
            let (actual, codec) = tokio::task::spawn_blocking(move || {
                let mut info = probe(&path).map_err(|_| io::Error::other("decode failed"))?;
                let codec = video_codec(&path)?;
                info.duration_ms = decode_duration(&path, info.alpha, codec == "vp8")?;
                Ok::<_, io::Error>((info, codec))
            })
            .await
            .map_err(io::Error::other)??;
            if codec != "vp9" {
                return Err(io::Error::other("codec changed"));
            }
            if actual.audio_channels.is_some() {
                let path = output.to_owned();
                let codec = tokio::task::spawn_blocking(move || audio_codec(&path))
                    .await
                    .map_err(io::Error::other)??;
                if codec != "opus" {
                    return Err(io::Error::other("audio track changed"));
                }
            }
            if (actual.width, actual.height) != (video.source.width, video.source.height) {
                return Err(io::Error::other("dimensions changed"));
            }
            if actual.audio_channels != video.source.audio_channels {
                return Err(io::Error::other("audio track changed"));
            }
            if video.source.alpha && !actual.alpha {
                return Err(io::Error::other("alpha lost"));
            }
            let (n, d) = video
                .source
                .fps
                .split_once('/')
                .and_then(|(n, d)| Some((n.parse::<f64>().ok()?, d.parse::<f64>().ok()?)))
                .unwrap_or((10.0, 1.0));
            let tolerance = (1000.0 * d / n).max(100.0) as u64;
            if actual.duration_ms.abs_diff(video.source.duration_ms) > tolerance {
                return Err(io::Error::other("duration changed"));
            }
            Ok(())
        })
    }
}
fn video_codec(path: &Path) -> io::Result<String> {
    let result = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()?;
    if !result.status.success() {
        return Err(io::Error::other("decode failed"));
    }
    Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

fn audio_codec(path: &Path) -> io::Result<String> {
    let result = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()?;
    if !result.status.success() {
        return Err(io::Error::other("decode failed"));
    }
    Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn generate(path: &Path, options: &[&str]) -> io::Result<()> {
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x48:rate=10:duration=1",
            ])
            .args(options)
            .arg("-y")
            .arg(path)
            .status()?;
        assert!(status.success(), "{}", path.display());
        Ok(())
    }
    #[tokio::test]
    async fn converts_video_and_rejects_invalid_output() -> io::Result<()> {
        use compressor_core::{registry::Transformer, ConvertSettings, VideoEncode};
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source.mp4");
        assert!(Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x48:rate=10:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=1",
                "-shortest",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                "-y"
            ])
            .arg(&source)
            .status()?
            .success());
        let props = probe(&source)?;
        let settings = ConvertSettings::Video(VideoEncode {
            crf: 36,
            cpu_used: 2,
            opus_kbps: Some(48),
            source: props,
        });
        let output = dir.path().join("output.mp4");
        let token = tokio_util::sync::CancellationToken::new();
        VideoTransformer
            .transform(&source, &output, &settings, &token)
            .await?;
        VideoTransformer
            .validate(&source, &output, &settings, &token)
            .await?;
        assert!(std::fs::read(&output)?.starts_with(b"\x1a\x45\xdf\xa3"));
        let silent = dir.path().join("silent.webm");
        generate(&silent, &["-c:v", "libvpx-vp9"])?;
        assert!(VideoTransformer
            .validate(&source, &silent, &settings, &token)
            .await
            .unwrap_err()
            .to_string()
            .contains("audio track changed"));
        std::fs::write(&output, b"\x1a\x45\xdf\xa3broken")?;
        assert!(VideoTransformer
            .validate(&source, &output, &settings, &token)
            .await
            .unwrap_err()
            .to_string()
            .contains("decode failed"));
        Ok(())
    }
    #[tokio::test]
    async fn converts_each_supported_container() -> io::Result<()> {
        use compressor_core::{registry::Transformer, ConvertSettings, VideoEncode};
        let dir = tempfile::tempdir()?;
        for (name, options) in [
            ("source.webm", vec!["-c:v", "libvpx-vp9"]),
            (
                "source.avi",
                vec!["-c:v", "rawvideo", "-pix_fmt", "yuv420p"],
            ),
            ("source.ogv", vec!["-c:v", "libtheora"]),
            ("live.webm", vec!["-c:v", "libvpx-vp9", "-live", "1"]),
        ] {
            let source = dir.path().join(name);
            generate(&source, &options)?;
            let settings = ConvertSettings::Video(VideoEncode {
                crf: 36,
                cpu_used: 2,
                opus_kbps: None,
                source: probe(&source)?,
            });
            let output = dir.path().join(format!("out-{name}"));
            let token = tokio_util::sync::CancellationToken::new();
            VideoTransformer
                .transform(&source, &output, &settings, &token)
                .await?;
            VideoTransformer
                .validate(&source, &output, &settings, &token)
                .await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn transparent_video_keeps_alpha() -> io::Result<()> {
        use compressor_core::{registry::Transformer, ConvertSettings, VideoEncode};
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source.webm");
        assert!(Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red@0.0:s=64x48:r=10:d=1,format=yuva420p",
                "-c:v",
                "libvpx-vp9",
                "-auto-alt-ref",
                "0",
                "-y"
            ])
            .arg(&source)
            .status()?
            .success());
        let settings = ConvertSettings::Video(VideoEncode {
            crf: 36,
            cpu_used: 2,
            opus_kbps: None,
            source: probe(&source)?,
        });
        let output = dir.path().join("output.webm");
        let token = tokio_util::sync::CancellationToken::new();
        VideoTransformer
            .transform(&source, &output, &settings, &token)
            .await?;
        VideoTransformer
            .validate(&source, &output, &settings, &token)
            .await?;
        let pixels = Command::new("ffmpeg")
            .args(["-v", "error", "-c:v", "libvpx-vp9", "-i"])
            .arg(&output)
            .args([
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "pipe:1",
            ])
            .output()?;
        assert!(pixels.status.success());
        assert!(pixels.stdout.as_chunks::<4>().0.iter().all(|p| p[3] == 0));
        let opaque = dir.path().join("opaque.webm");
        generate(&opaque, &["-c:v", "libvpx-vp9"])?;
        assert!(VideoTransformer
            .validate(&source, &opaque, &settings, &token)
            .await
            .unwrap_err()
            .to_string()
            .contains("alpha lost"));
        Ok(())
    }
    #[test]
    fn probes_video_containers_and_rejects_bad_tracks() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        for (name, options) in [
            ("video.webm", vec!["-c:v", "libvpx-vp9"]),
            ("video.mp4", vec!["-c:v", "libx264"]),
            ("video.avi", vec!["-c:v", "rawvideo", "-pix_fmt", "yuv420p"]),
            ("video.ogv", vec!["-c:v", "libtheora"]),
            ("noduration.webm", vec!["-c:v", "libvpx-vp9", "-live", "1"]),
        ] {
            let path = dir.path().join(name);
            generate(&path, &options)?;
            let props = probe(&path)?;
            assert_eq!(
                (props.width, props.height, props.audio_channels),
                (64, 48, None)
            );
            assert!(
                (900..=1100).contains(&props.duration_ms),
                "{name}: {}",
                props.duration_ms
            );
        }
        let alpha = dir.path().join("alpha.webm");
        assert!(Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red@0.0:s=64x48:r=10:d=1,format=yuva420p",
                "-c:v",
                "libvpx-vp9",
                "-auto-alt-ref",
                "0",
                "-y"
            ])
            .arg(&alpha)
            .status()?
            .success());
        assert!(probe(&alpha)?.alpha);
        let mut broken = std::fs::read(dir.path().join("noduration.webm"))?;
        let cluster = broken
            .windows(4)
            .position(|w| w == b"\x1f\x43\xb6\x75")
            .expect("cluster");
        broken[cluster + 40..].fill(0xff);
        let broken_path = dir.path().join("broken.webm");
        std::fs::write(&broken_path, broken)?;
        let error = probe(&broken_path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
        let movie = dir.path().join("with-audio.mp4");
        assert!(Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x48:rate=10:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=1",
                "-shortest",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                "-y"
            ])
            .arg(&movie)
            .status()?
            .success());
        assert_eq!(probe(&movie)?.audio_channels, Some(1));
        let audio = dir.path().join("audio.webm");
        assert!(Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=1",
                "-c:a",
                "libopus",
                "-y"
            ])
            .arg(&audio)
            .status()?
            .success());
        assert!(probe(&audio)
            .unwrap_err()
            .to_string()
            .contains("unsupported video format"));
        let surround = dir.path().join("surround.mp4");
        assert!(Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x48:rate=10:duration=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=channel_layout=5.1:sample_rate=44100",
                "-shortest",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                "-y"
            ])
            .arg(&surround)
            .status()?
            .success());
        assert!(probe(&surround)
            .unwrap_err()
            .to_string()
            .contains("unsupported audio track"));
        Ok(())
    }
}
