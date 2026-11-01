//! Tool-free audio container inspection for planning.
use compressor_core::{
    process::run_process,
    registry::{Operation, Transformer},
    AudioCodec, ConvertSettings, MediaKind, ToolSpec,
};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};
use tokio_util::sync::CancellationToken;

pub struct AudioTransformer;
impl AudioTransformer {
    pub async fn validate_output(
        output: &Path,
        settings: &ConvertSettings,
        cancel: &CancellationToken,
        force: &CancellationToken,
    ) -> io::Result<()> {
        let ConvertSettings::Audio(settings) = settings else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected audio settings",
            ));
        };
        let args = [
            "-v".as_ref(),
            "error".as_ref(),
            "-show_streams".as_ref(),
            "-show_format".as_ref(),
            "-of".as_ref(),
            "json".as_ref(),
            "-o".as_ref(),
            "pipe:2".as_ref(),
            output.as_os_str(),
        ];
        let probe = run_process("ffprobe".as_ref(), args, cancel, force).await?;
        if probe.cancelled {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        if !probe.status.success() {
            return Err(io::Error::other("decode failed"));
        }
        let json: serde_json::Value =
            serde_json::from_slice(&probe.stderr).map_err(|_| io::Error::other("decode failed"))?;
        let stream = json["streams"]
            .as_array()
            .and_then(|s| s.iter().find(|s| s["codec_type"] == "audio"))
            .ok_or_else(|| io::Error::other("decode failed"))?;
        let codec = match settings.codec {
            AudioCodec::Opus => "opus",
            AudioCodec::Vorbis => "vorbis",
            AudioCodec::Mp3 => "mp3",
        };
        if stream["codec_name"].as_str() != Some(codec) {
            return Err(io::Error::other("codec changed"));
        }
        if stream["channels"].as_u64() != Some(u64::from(settings.source.channels)) {
            return Err(io::Error::other("channels changed"));
        }
        if settings.codec != AudioCodec::Opus
            && stream["sample_rate"]
                .as_str()
                .and_then(|s| s.parse::<u32>().ok())
                != Some(settings.source.sample_rate)
        {
            return Err(io::Error::other("sample rate changed"));
        }
        let duration = json["format"]["duration"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .ok_or_else(|| io::Error::other("decode failed"))?;
        if ((duration * 1000.0).round() - settings.source.duration_ms as f64).abs() > 100.0 {
            return Err(io::Error::other("duration changed"));
        }
        let args = [
            "-nostdin".as_ref(),
            "-v".as_ref(),
            "error".as_ref(),
            "-xerror".as_ref(),
            "-i".as_ref(),
            output.as_os_str(),
            "-f".as_ref(),
            "null".as_ref(),
            "-".as_ref(),
        ];
        let decoded = run_process("ffmpeg".as_ref(), args, cancel, force).await?;
        if decoded.cancelled {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        if !decoded.status.success() || !decoded.stderr.is_empty() {
            return Err(io::Error::other("decode failed"));
        }
        Ok(())
    }
}
impl Transformer for AudioTransformer {
    fn kind(&self) -> MediaKind {
        MediaKind::Audio
    }
    fn required_tools(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "ffmpeg".into(),
                package: "ffmpeg".into(),
                features: vec![],
            },
            ToolSpec {
                name: "ffprobe".into(),
                package: "ffmpeg".into(),
                features: vec![],
            },
        ]
    }
    fn required_tools_for(&self, settings: &ConvertSettings) -> Vec<ToolSpec> {
        let mut tools = self.required_tools();
        if let ConvertSettings::Audio(audio) = settings {
            tools[0].features.push(
                match audio.codec {
                    AudioCodec::Opus => "libopus",
                    AudioCodec::Vorbis => "libvorbis",
                    AudioCodec::Mp3 => "libmp3lame",
                }
                .into(),
            );
        }
        tools
    }
    fn transform<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        settings: &'a ConvertSettings,
        cancel: &'a CancellationToken,
    ) -> Operation<'a, Option<String>> {
        Box::pin(async move {
            self.transform_with_force(source, output, settings, cancel, &CancellationToken::new())
                .await
        })
    }
    fn transform_with_force<'a>(
        &'a self,
        source: &'a Path,
        output: &'a Path,
        settings: &'a ConvertSettings,
        cancel: &'a CancellationToken,
        force: &'a CancellationToken,
    ) -> Operation<'a, Option<String>> {
        Box::pin(async move {
            let ConvertSettings::Audio(settings) = settings else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "expected audio settings",
                ));
            };
            if !source.is_absolute() || !output.is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "encoder paths must be absolute",
                ));
            }
            let info = sniff(source)?;
            let mut args = vec![
                "-nostdin".into(),
                "-hide_banner".into(),
                "-v".into(),
                "error".into(),
                "-threads".into(),
                "1".into(),
                "-i".into(),
                source.as_os_str().to_owned(),
                "-map".into(),
                "0:a:0".into(),
                "-vn".into(),
                "-map_metadata".into(),
                match info.container {
                    Container::Ogg => "0:s:a:0",
                    Container::Mp3 => "0",
                }
                .into(),
            ];
            let bitrate = || {
                settings
                    .bitrate_kbps
                    .map(|n| format!("{n}k"))
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing bitrate"))
            };
            match settings.codec {
                AudioCodec::Opus => args.extend([
                    "-c:a".into(),
                    "libopus".into(),
                    "-b:a".into(),
                    bitrate()?.into(),
                    "-vbr".into(),
                    "on".into(),
                    "-compression_level".into(),
                    "10".into(),
                    "-f".into(),
                    "ogg".into(),
                ]),
                AudioCodec::Vorbis => args.extend([
                    "-c:a".into(),
                    "libvorbis".into(),
                    "-q:a".into(),
                    settings
                        .vorbis_quality
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "missing Vorbis quality")
                        })?
                        .to_string()
                        .into(),
                    "-f".into(),
                    "ogg".into(),
                ]),
                AudioCodec::Mp3 => args.extend([
                    "-c:a".into(),
                    "libmp3lame".into(),
                    "-b:a".into(),
                    bitrate()?.into(),
                    "-id3v2_version".into(),
                    "3".into(),
                    "-write_xing".into(),
                    "1".into(),
                    "-f".into(),
                    "mp3".into(),
                ]),
            }
            args.extend(["-y".into(), output.as_os_str().to_owned()]);
            let result = run_process("ffmpeg".as_ref(), args, cancel, force).await?;
            if result.cancelled {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            if !result.status.success() {
                return Err(io::Error::other(format!(
                    "ffmpeg: {}",
                    String::from_utf8_lossy(&result.stderr)
                )));
            }
            Ok(info
                .embedded_picture
                .then(|| "embedded picture dropped".into()))
        })
    }
    fn validate<'a>(
        &'a self,
        _source: &'a Path,
        output: &'a Path,
        settings: &'a ConvertSettings,
        cancel: &'a CancellationToken,
    ) -> Operation<'a, ()> {
        Box::pin(async move {
            Self::validate_output(output, settings, cancel, &CancellationToken::new()).await
        })
    }
    fn validate_with_force<'a>(
        &'a self,
        _source: &'a Path,
        output: &'a Path,
        settings: &'a ConvertSettings,
        cancel: &'a CancellationToken,
        force: &'a CancellationToken,
    ) -> Operation<'a, ()> {
        Box::pin(Self::validate_output(output, settings, cancel, force))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    Ogg,
    Mp3,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Vorbis,
    Opus,
    Mp3,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioInfo {
    pub container: Container,
    pub codec: Codec,
    pub channels: u32,
    pub sample_rate: u32,
    pub duration_ms: u64,
    pub avg_kbps: u32,
    pub embedded_picture: bool,
}

fn unsupported() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "unsupported audio format")
}

pub fn sniff(path: &Path) -> io::Result<AudioInfo> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut magic = [0; 4];
    file.read_exact(&mut magic).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            unsupported()
        } else {
            e
        }
    })?;
    file.seek(SeekFrom::Start(0))?;
    if &magic == b"OggS" {
        sniff_ogg(&mut file, size)
    } else {
        sniff_mp3(&mut file, size)
    }
}

fn synchsafe(bytes: &[u8]) -> Option<u32> {
    if bytes.len() != 4 || bytes.iter().any(|v| v & 0x80 != 0) {
        return None;
    }
    Some(bytes.iter().fold(0, |n, b| (n << 7) | u32::from(*b)))
}

/// How far past the ID3 tag to look for the first MP3 frame.
const SYNC_SEARCH: u64 = 65536;
/// Larger than any MPEG-1/2/2.5 Layer III frame.
const MAX_FRAME: u64 = 2881;
/// Consecutive frames required to accept a sync position.
const CHAIN: usize = 4;

#[derive(Clone, Copy)]
struct Mp3Frame {
    mpeg1: bool,
    crc: bool,
    sample_rate: u32,
    channels: u32,
    length: u64,
    samples: u64,
}

impl Mp3Frame {
    /// Parse an MPEG audio Layer III frame header.
    fn parse(h: u32) -> Option<Self> {
        let version = (h >> 19) & 3;
        let layer = (h >> 17) & 3;
        let bitrate_index = ((h >> 12) & 15) as usize;
        let rate_index = ((h >> 10) & 3) as usize;
        if h & 0xffe00000 != 0xffe00000
            || version == 1
            || layer != 1
            || bitrate_index == 0
            || bitrate_index == 15
            || rate_index == 3
        {
            return None;
        }
        let mpeg1 = version == 3;
        let sample_rate = [44100u32, 48000, 32000][rate_index]
            / match version {
                2 => 2,
                0 => 4,
                _ => 1,
            };
        let kbps = if mpeg1 {
            [
                0u32, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
            ][bitrate_index]
        } else {
            [
                0u32, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160,
            ][bitrate_index]
        };
        let length =
            u64::from((if mpeg1 { 144000 } else { 72000 }) * kbps / sample_rate + ((h >> 9) & 1));
        Some(Self {
            mpeg1,
            crc: (h >> 16) & 1 == 0,
            sample_rate,
            channels: if (h >> 6) & 3 == 3 { 1 } else { 2 },
            length,
            samples: if mpeg1 { 1152 } else { 576 },
        })
    }
}

/// Whether `CHAIN` consistent frames start at `start`, or fewer that end
/// exactly at the end of a short stream of `remaining` bytes.
fn frame_chain(window: &[u8], start: usize, remaining: u64) -> bool {
    let mut pos = start;
    let mut first: Option<Mp3Frame> = None;
    for _ in 0..CHAIN {
        if pos as u64 == remaining && first.is_some() {
            return true;
        }
        let Some(bytes) = window.get(pos..pos + 4) else {
            return false;
        };
        let Some(frame) = Mp3Frame::parse(u32::from_be_bytes(bytes.try_into().unwrap())) else {
            return false;
        };
        if first.is_some_and(|f| {
            (f.mpeg1, f.sample_rate, f.channels) != (frame.mpeg1, frame.sample_rate, frame.channels)
        }) || pos as u64 + frame.length > remaining
        {
            return false;
        }
        first.get_or_insert(frame);
        pos += frame.length as usize;
    }
    true
}

fn sniff_mp3(file: &mut File, size: u64) -> io::Result<AudioInfo> {
    let mut offset = 0u64;
    let mut picture = false;
    if size >= 10 {
        let mut id3 = [0u8; 10];
        file.read_exact(&mut id3)?;
        if &id3[..3] == b"ID3" {
            let tag_size = u64::from(synchsafe(&id3[6..10]).ok_or_else(unsupported)?);
            offset = 10
                + tag_size
                + if id3[3] == 4 && id3[5] & 0x10 != 0 {
                    10
                } else {
                    0
                };
            if offset > size {
                return Err(unsupported());
            }
            let mut frame_pos = 10u64;
            if id3[5] & 0x40 != 0 && tag_size >= 4 {
                let mut ext = [0u8; 4];
                file.read_exact(&mut ext)?;
                let length = if id3[3] == 4 {
                    u64::from(synchsafe(&ext).ok_or_else(unsupported)?)
                } else {
                    u64::from(u32::from_be_bytes(ext)) + 4
                };
                frame_pos += length;
            }
            while frame_pos + 10 <= 10 + tag_size && matches!(id3[3], 3 | 4) {
                file.seek(SeekFrom::Start(frame_pos))?;
                let mut h = [0u8; 10];
                file.read_exact(&mut h)?;
                if h[..4].iter().all(|b| *b == 0) {
                    break;
                }
                let frame_size = if id3[3] == 4 {
                    u64::from(synchsafe(&h[4..8]).ok_or_else(unsupported)?)
                } else {
                    u64::from(u32::from_be_bytes(h[4..8].try_into().unwrap()))
                };
                if frame_pos + 10 + frame_size > 10 + tag_size {
                    return Err(unsupported());
                }
                if &h[..4] == b"APIC" {
                    picture = true;
                }
                frame_pos += 10 + frame_size;
            }
        }
    }
    let audio_start = offset;
    // Find the first frame that starts a chain of consistent frames. A lone
    // sync pattern is common in arbitrary binary data, so it is not enough.
    let mut window = Vec::new();
    file.seek(SeekFrom::Start(audio_start))?;
    file.by_ref()
        .take(SYNC_SEARCH + 4 * MAX_FRAME)
        .read_to_end(&mut window)?;
    let start = (0..window.len().min(SYNC_SEARCH as usize))
        .find(|&i| frame_chain(&window, i, size - audio_start))
        .ok_or_else(unsupported)?;
    let mut samples = 0u64;
    let mut first: Option<Mp3Frame> = None;
    let mut gapless_samples = 0u64;
    let mut pos = audio_start + start as u64;
    let mut reader = io::BufReader::new(&mut *file);
    reader.seek(SeekFrom::Start(pos))?;
    while pos + 4 <= size {
        let mut bytes = [0u8; 4];
        reader.read_exact(&mut bytes)?;
        let Some(frame) = Mp3Frame::parse(u32::from_be_bytes(bytes)) else {
            break;
        };
        if pos + frame.length > size {
            break;
        }
        if let Some(old) = first {
            if (frame.sample_rate, frame.channels) != (old.sample_rate, old.channels) {
                return Err(unsupported());
            }
            reader.seek_relative(frame.length as i64 - 4)?;
        } else {
            first = Some(frame);
            // The Xing/Info frame contains metadata, not audio. LAME-compatible
            // tags also record encoder delay and padding, which are not played.
            let side = match (frame.mpeg1, frame.channels) {
                (true, 1) => 17,
                (true, _) => 32,
                (false, 1) => 9,
                (false, _) => 17,
            };
            let xing_offset = 4 + if frame.crc { 2 } else { 0 } + side;
            let mut data = vec![0; frame.length as usize];
            data[..4].copy_from_slice(&bytes);
            reader.read_exact(&mut data[4..])?;
            if data
                .get(xing_offset..xing_offset + 4)
                .is_some_and(|tag| tag == b"Xing" || tag == b"Info")
            {
                gapless_samples = frame.samples;
                if let Some(flags) = data.get(xing_offset + 4..xing_offset + 8) {
                    let flags = u32::from_be_bytes(flags.try_into().unwrap());
                    let tag = xing_offset
                        + 8
                        + if flags & 1 != 0 { 4 } else { 0 }
                        + if flags & 2 != 0 { 4 } else { 0 }
                        + if flags & 4 != 0 { 100 } else { 0 }
                        + if flags & 8 != 0 { 4 } else { 0 };
                    if data
                        .get(tag..tag + 4)
                        .is_some_and(|name| name == b"LAME" || name == b"Lavc")
                    {
                        if let Some(padding) = data.get(tag + 21..tag + 24) {
                            let delay = (u64::from(padding[0]) << 4) | u64::from(padding[1] >> 4);
                            let end_padding =
                                (u64::from(padding[1] & 15) << 8) | u64::from(padding[2]);
                            gapless_samples += delay + end_padding;
                        }
                    }
                }
            }
        }
        samples += frame.samples;
        pos += frame.length;
    }
    let (rate, channels) = first
        .map(|f| (f.sample_rate, f.channels))
        .ok_or_else(unsupported)?;
    samples = samples
        .checked_sub(gapless_samples)
        .filter(|s| *s > 0)
        .ok_or_else(unsupported)?;
    let duration_ms = (samples * 1000 + u64::from(rate) / 2) / u64::from(rate);
    let avg_kbps = ((u128::from(size - audio_start) * 8 * u128::from(rate) / u128::from(samples)
        + 500)
        / 1000)
        .min(u128::from(u32::MAX)) as u32;
    Ok(AudioInfo {
        container: Container::Mp3,
        codec: Codec::Mp3,
        channels,
        sample_rate: rate,
        duration_ms,
        avg_kbps,
        embedded_picture: picture,
    })
}

fn sniff_ogg(file: &mut File, size: u64) -> io::Result<AudioInfo> {
    let mut identification = Vec::new();
    let mut packet = 0u8;
    let mut comment_tail = Vec::new();
    let mut picture = false;
    let mut serial = None;
    let mut granule = None;
    loop {
        if file.stream_position()? == size {
            break;
        }
        let mut header = [0; 27];
        match file.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) => return Err(e),
        }
        if &header[..4] != b"OggS" || header[4] != 0 {
            return Err(unsupported());
        }
        let page_serial = u32::from_le_bytes(header[14..18].try_into().unwrap());
        if serial.is_none() {
            serial = Some(page_serial);
        }
        let position = u64::from_le_bytes(header[6..14].try_into().unwrap());
        let mut lacing = vec![0; header[26] as usize];
        file.read_exact(&mut lacing)?;
        let payload_size: usize = lacing.iter().map(|x| *x as usize).sum();
        let mut payload = vec![0; payload_size];
        file.read_exact(&mut payload)?;
        if Some(page_serial) != serial {
            continue;
        }
        if position != u64::MAX {
            granule = Some(position);
        }
        if packet < 2 {
            let mut offset = 0;
            for len in lacing {
                let len = len as usize;
                if packet == 0 {
                    identification.extend_from_slice(&payload[offset..offset + len]);
                    if identification.len() > 65536 {
                        return Err(unsupported());
                    }
                } else if packet == 1 {
                    comment_tail.extend(
                        payload[offset..offset + len]
                            .iter()
                            .map(u8::to_ascii_lowercase),
                    );
                    picture |= comment_tail
                        .windows(b"metadata_block_picture=".len())
                        .any(|w| w == b"metadata_block_picture=")
                        || comment_tail
                            .windows(b"coverart=".len())
                            .any(|w| w == b"coverart=");
                    let trim = comment_tail.len().saturating_sub(24);
                    comment_tail.drain(..trim);
                }
                offset += len;
                if len < 255 {
                    packet += 1;
                    if packet >= 2 {
                        break;
                    }
                }
            }
        }
    }
    let (codec, channels, rate, preskip) =
        if identification.starts_with(b"\x01vorbis") && identification.len() >= 16 {
            (
                Codec::Vorbis,
                u32::from(identification[11]),
                u32::from_le_bytes(identification[12..16].try_into().unwrap()),
                0,
            )
        } else if identification.starts_with(b"OpusHead") && identification.len() >= 19 {
            (
                Codec::Opus,
                u32::from(identification[9]),
                48000,
                u64::from(u16::from_le_bytes(
                    identification[10..12].try_into().unwrap(),
                )),
            )
        } else {
            return Err(unsupported());
        };
    if channels == 0 || rate == 0 {
        return Err(unsupported());
    }
    let samples = granule
        .ok_or_else(unsupported)?
        .checked_sub(preskip)
        .ok_or_else(unsupported)?;
    if samples == 0 {
        return Err(unsupported());
    }
    let duration_ms = (samples * 1000 + u64::from(rate) / 2) / u64::from(rate);
    let avg_kbps = ((u128::from(size) * 8 * u128::from(rate) / u128::from(samples) + 500) / 1000)
        .min(u128::from(u32::MAX)) as u32;
    Ok(AudioInfo {
        container: Container::Ogg,
        codec,
        channels,
        sample_rate: rate,
        duration_ms,
        avg_kbps,
        embedded_picture: picture,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    #[tokio::test]
    async fn encodes_each_codec_into_its_container() -> io::Result<()> {
        use compressor_core::{AudioEncode, AudioProps};
        let dir = tempfile::tempdir()?;
        for (codec, expected, ext) in [
            (AudioCodec::Opus, Codec::Opus, "ogg"),
            (AudioCodec::Vorbis, Codec::Vorbis, "ogg"),
            (AudioCodec::Mp3, Codec::Mp3, "mp3"),
        ] {
            let source = dir.path().join(format!("input-{ext}.{ext}"));
            if !source.exists() {
                let status = Command::new("ffmpeg")
                    .args([
                        "-nostdin",
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-f",
                        "lavfi",
                        "-i",
                        "sine=duration=1",
                        "-c:a",
                        if ext == "mp3" {
                            "libmp3lame"
                        } else {
                            "libvorbis"
                        },
                        "-y",
                    ])
                    .arg(&source)
                    .status()?;
                assert!(status.success());
            }
            let source_info = sniff(&source)?;
            let settings = ConvertSettings::Audio(AudioEncode {
                codec,
                bitrate_kbps: Some(64),
                vorbis_quality: Some(3),
                source: AudioProps {
                    channels: source_info.channels,
                    sample_rate: source_info.sample_rate,
                    duration_ms: source_info.duration_ms,
                },
            });
            let output = dir.path().join(format!("output-{expected:?}.stage"));
            AudioTransformer
                .transform(&source, &output, &settings, &CancellationToken::new())
                .await?;
            AudioTransformer
                .validate(&source, &output, &settings, &CancellationToken::new())
                .await?;
            let info = sniff(&output)?;
            assert_eq!(info.codec, expected);
            assert_eq!(
                info.container,
                if ext == "mp3" {
                    Container::Mp3
                } else {
                    Container::Ogg
                }
            );
        }
        Ok(())
    }
    #[tokio::test]
    async fn validator_rejects_corruption_channels_and_duration() -> io::Result<()> {
        use compressor_core::{AudioEncode, AudioProps};
        let dir = tempfile::tempdir()?;
        let output = dir.path().join("mono.ogg");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=2",
                "-ac",
                "1",
                "-c:a",
                "libvorbis",
                "-y",
            ])
            .arg(&output)
            .status()?;
        assert!(status.success());
        let mut settings = AudioEncode {
            codec: AudioCodec::Vorbis,
            bitrate_kbps: None,
            vorbis_quality: Some(3),
            source: AudioProps {
                channels: 1,
                sample_rate: 44100,
                duration_ms: 2000,
            },
        };
        let check = |settings: &AudioEncode| ConvertSettings::Audio(settings.clone());
        let token = CancellationToken::new();
        AudioTransformer
            .validate(&output, &output, &check(&settings), &token)
            .await?;
        settings.source.channels = 2;
        assert!(AudioTransformer
            .validate(&output, &output, &check(&settings), &token)
            .await
            .unwrap_err()
            .to_string()
            .contains("channels changed"));
        settings.source.channels = 1;
        settings.source.duration_ms = 3000;
        assert!(AudioTransformer
            .validate(&output, &output, &check(&settings), &token)
            .await
            .unwrap_err()
            .to_string()
            .contains("duration changed"));
        settings.source.duration_ms = 2000;
        let bytes = std::fs::read(&output)?;
        std::fs::write(&output, &bytes[..bytes.len() / 2])?;
        assert!(AudioTransformer
            .validate(&output, &output, &check(&settings), &token)
            .await
            .unwrap_err()
            .to_string()
            .contains("decode failed"));
        Ok(())
    }
    #[tokio::test]
    async fn preserves_loop_tags_and_reports_dropped_cover() -> io::Result<()> {
        use compressor_core::{AudioEncode, AudioProps};
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("loop.ogg");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=1",
                "-c:a",
                "libvorbis",
                "-metadata",
                "LOOPSTART=123",
                "-metadata",
                "LOOPLENGTH=456",
                "-y",
            ])
            .arg(&source)
            .status()?;
        assert!(status.success());
        let info = sniff(&source)?;
        for codec in [AudioCodec::Opus, AudioCodec::Vorbis] {
            let settings = ConvertSettings::Audio(AudioEncode {
                codec: codec.clone(),
                bitrate_kbps: Some(64),
                vorbis_quality: Some(3),
                source: AudioProps {
                    channels: info.channels,
                    sample_rate: info.sample_rate,
                    duration_ms: info.duration_ms,
                },
            });
            let output = dir.path().join(format!("{codec:?}.ogg"));
            assert_eq!(
                AudioTransformer
                    .transform(&source, &output, &settings, &CancellationToken::new())
                    .await?,
                None
            );
            let tags = Command::new("ffprobe")
                .args(["-v", "error", "-show_entries", "stream_tags", "-of", "json"])
                .arg(&output)
                .output()?;
            assert!(tags.status.success());
            let value: serde_json::Value = serde_json::from_slice(&tags.stdout)?;
            let tags = value["streams"][0]["tags"].as_object().unwrap();
            assert!(tags
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("LOOPSTART") && v == "123"));
            assert!(tags
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("LOOPLENGTH") && v == "456"));
        }
        let fixture = compressor_test_fixtures::Fixture::new()?;
        use base64::Engine as _;
        let image = std::fs::read(fixture.root().join("game/icon.png"))?;
        let mut block = Vec::new();
        block.extend(3u32.to_be_bytes());
        block.extend(9u32.to_be_bytes());
        block.extend(b"image/png");
        block.extend(0u32.to_be_bytes());
        for value in [64u32, 64, 32, 0, image.len() as u32] {
            block.extend(value.to_be_bytes());
        }
        block.extend(image);
        let picture_tag = format!(
            "METADATA_BLOCK_PICTURE={}",
            base64::engine::general_purpose::STANDARD.encode(block)
        );
        let ogg_picture = dir.path().join("cover.ogg");
        let status = Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(&source)
            .args(["-c:a", "libvorbis", "-metadata"])
            .arg(picture_tag)
            .args(["-y"])
            .arg(&ogg_picture)
            .status()?;
        assert!(status.success());
        let info = sniff(&ogg_picture)?;
        assert!(info.embedded_picture);
        let settings = ConvertSettings::Audio(AudioEncode {
            codec: AudioCodec::Opus,
            bitrate_kbps: Some(64),
            vorbis_quality: None,
            source: AudioProps {
                channels: info.channels,
                sample_rate: info.sample_rate,
                duration_ms: info.duration_ms,
            },
        });
        let output = dir.path().join("no-cover.ogg");
        assert_eq!(
            AudioTransformer
                .transform(&ogg_picture, &output, &settings, &CancellationToken::new())
                .await?
                .as_deref(),
            Some("embedded picture dropped")
        );
        assert!(!sniff(&output)?.embedded_picture);
        let picture = dir.path().join("cover.mp3");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=1",
                "-i",
            ])
            .arg(fixture.root().join("game/icon.png"))
            .args([
                "-map",
                "0:a",
                "-map",
                "1:v",
                "-c:a",
                "libmp3lame",
                "-c:v",
                "copy",
                "-id3v2_version",
                "3",
                "-y",
            ])
            .arg(&picture)
            .status()?;
        assert!(status.success());
        let info = sniff(&picture)?;
        assert!(info.embedded_picture);
        let settings = ConvertSettings::Audio(AudioEncode {
            codec: AudioCodec::Mp3,
            bitrate_kbps: Some(64),
            vorbis_quality: None,
            source: AudioProps {
                channels: info.channels,
                sample_rate: info.sample_rate,
                duration_ms: info.duration_ms,
            },
        });
        let output = dir.path().join("no-cover.mp3");
        let note = AudioTransformer
            .transform(&picture, &output, &settings, &CancellationToken::new())
            .await?;
        assert_eq!(note.as_deref(), Some("embedded picture dropped"));
        assert!(!sniff(&output)?.embedded_picture);
        let streams = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type",
                "-of",
                "json",
            ])
            .arg(output)
            .output()?;
        let value: serde_json::Value = serde_json::from_slice(&streams.stdout)?;
        assert!(value["streams"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["codec_type"] != "video"));
        Ok(())
    }
    #[test]
    fn binary_with_lone_frame_syncs_is_not_mp3() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("data.rpyc");
        // Valid-looking MPEG-1 Layer III headers that do not chain into frames.
        let mut data = vec![0u8; 200_000];
        for chunk in data.chunks_mut(997) {
            chunk[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
        }
        std::fs::write(&path, data)?;
        let err = sniff(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        Ok(())
    }
    #[test]
    fn sniffs_mp3_cbr_vbr_and_picture() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        for (name, quality) in [("cbr", vec!["-b:a", "128k"]), ("vbr", vec!["-q:a", "3"])] {
            let path = dir.path().join(format!("{name}.mp3"));
            let status = Command::new("ffmpeg")
                .args([
                    "-nostdin",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=44100:duration=2",
                    "-ac",
                    "2",
                    "-c:a",
                    "libmp3lame",
                ])
                .args(quality)
                .arg("-y")
                .arg(&path)
                .status()?;
            assert!(status.success());
            let info = sniff(&path)?;
            assert_eq!(info.codec, Codec::Mp3);
            assert_eq!(info.channels, 2);
            assert_eq!(info.sample_rate, 44100);
            assert!(info.duration_ms.abs_diff(2000) < 100, "{info:?}");
            assert!(!info.embedded_picture);
            if name == "cbr" {
                assert!((120..=136).contains(&info.avg_kbps), "{info:?}");
            }
        }
        let fixture = compressor_test_fixtures::Fixture::new()?;
        let with_picture = dir.path().join("picture.mp3");
        let status = Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(dir.path().join("cbr.mp3"))
            .arg("-i")
            .arg(fixture.root().join("game/icon.png"))
            .args([
                "-map",
                "0:a",
                "-map",
                "1:v",
                "-c",
                "copy",
                "-id3v2_version",
                "3",
                "-y",
            ])
            .arg(&with_picture)
            .status()?;
        assert!(status.success());
        assert!(sniff(&with_picture)?.embedded_picture);
        Ok(())
    }
    #[test]
    fn mp3_duration_accounts_for_encoder_delay_and_padding() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("short.mp3");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=sample_rate=32000:duration=0.612",
                "-ac",
                "1",
                "-c:a",
                "libmp3lame",
                "-b:a",
                "128k",
                "-y",
            ])
            .arg(&path)
            .status()?;
        assert!(status.success());
        let info = sniff(&path)?;
        assert!(info.duration_ms.abs_diff(612) <= 1, "{info:?}");
        Ok(())
    }
    #[test]
    fn sniffs_generated_ogg_streams() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        for (encoder, codec) in [("libvorbis", Codec::Vorbis), ("libopus", Codec::Opus)] {
            for channels in [1, 2] {
                let path = dir.path().join(format!("{encoder}-{channels}.ogg"));
                let status = Command::new("ffmpeg")
                    .args([
                        "-nostdin",
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-f",
                        "lavfi",
                        "-i",
                        "sine=frequency=440:sample_rate=44100:duration=1",
                        "-ac",
                        &channels.to_string(),
                        "-c:a",
                        encoder,
                        "-y",
                    ])
                    .arg(&path)
                    .status()?;
                assert!(status.success());
                let info = sniff(&path)?;
                assert_eq!(info.codec, codec);
                assert_eq!(info.channels, channels);
                assert_eq!(
                    info.sample_rate,
                    if codec == Codec::Opus { 48000 } else { 44100 }
                );
                assert!(info.duration_ms.abs_diff(1000) <= 1, "{info:?}");
                let expected = (std::fs::metadata(&path)?.len() * 8 + 500) / 1000;
                assert_eq!(u64::from(info.avg_kbps), expected);
            }
        }
        let path = dir.path().join("flac.ogg");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=1",
                "-c:a",
                "flac",
                "-f",
                "ogg",
                "-y",
            ])
            .arg(&path)
            .status()?;
        assert!(status.success());
        assert!(sniff(&path)
            .unwrap_err()
            .to_string()
            .contains("unsupported audio format"));
        Ok(())
    }
}
