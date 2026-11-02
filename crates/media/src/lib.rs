//! Content-based image detection and WebP conversion.

pub mod audio;
pub mod video;
pub mod webp;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Cursor, Read, Seek, SeekFrom},
    path::Path,
};

#[derive(Debug, Default, serde::Serialize, PartialEq, Eq)]
pub struct MediaCount {
    pub count: u64,
    pub bytes: u64,
}

/// Add a regular file to a content-based media summary.
pub fn summarize_file(
    summary: &mut BTreeMap<String, MediaCount>,
    path: &Path,
    size: u64,
) -> io::Result<()> {
    let kind = match detect(path)? {
        ImageFormat::Png => "png",
        ImageFormat::AnimatedPng => "apng",
        ImageFormat::Jpeg => "jpeg",
        ImageFormat::Webp => "webp",
        ImageFormat::AnimatedWebp => "animated_webp",
        ImageFormat::Unsupported => match audio::sniff(path) {
            Ok(info) => match info.codec {
                audio::Codec::Vorbis => "vorbis",
                audio::Codec::Opus => "opus",
                audio::Codec::Mp3 => "mp3",
                audio::Codec::Flac => "flac",
            },
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
                ) =>
            {
                if video::has_video_magic(path)? && video::probe(path).is_ok() {
                    "video"
                } else {
                    "other"
                }
            }
            Err(e) => return Err(e),
        },
    };
    let entry = summary.entry(kind.into()).or_default();
    entry.count += 1;
    entry.bytes += size;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    AnimatedPng,
    Jpeg,
    Webp,
    AnimatedWebp,
    Unsupported,
}

/// Detect image format from the contents, without trusting the filename.
pub fn detect(path: &Path) -> io::Result<ImageFormat> {
    detect_reader(&mut File::open(path)?)
}
pub fn detect_bytes(data: &[u8]) -> ImageFormat {
    detect_reader(&mut Cursor::new(data)).unwrap_or(ImageFormat::Unsupported)
}

pub fn detect_reader(reader: &mut (impl Read + Seek)) -> io::Result<ImageFormat> {
    let mut header = [0u8; 21];
    let mut count = 0;
    while count < header.len() {
        let n = reader.read(&mut header[count..])?;
        if n == 0 {
            break;
        }
        count += n;
    }
    if count >= 8 && header.starts_with(b"\x89PNG\r\n\x1a\n") {
        reader.seek(SeekFrom::Start(8))?;
        loop {
            let mut chunk = [0u8; 8];
            match reader.read_exact(&mut chunk) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let size = u32::from_be_bytes(chunk[..4].try_into().unwrap()) as i64;
            let kind = &chunk[4..];
            if kind == b"acTL" {
                return Ok(ImageFormat::AnimatedPng);
            }
            if kind == b"IDAT" || kind == b"IEND" {
                break;
            }
            reader.seek(SeekFrom::Current(size + 4))?;
        }
        Ok(ImageFormat::Png)
    } else if count >= 3 && header.starts_with(b"\xff\xd8\xff") {
        Ok(ImageFormat::Jpeg)
    } else if count >= 12 && &header[..4] == b"RIFF" && &header[8..12] == b"WEBP" {
        if count >= 21 && &header[12..16] == b"VP8X" && header[20] & 0x02 != 0 {
            Ok(ImageFormat::AnimatedWebp)
        } else {
            Ok(ImageFormat::Webp)
        }
    } else {
        Ok(ImageFormat::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compressor_test_fixtures::Fixture;
    #[test]
    fn summary_counts_mixed_audio_and_images() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let dir = tempfile::tempdir()?;
        let image = dir.path().join("image.png");
        std::fs::copy(fixture.root().join("game/icon.png"), &image)?;
        let audio = dir.path().join("music.ogg");
        let status = std::process::Command::new("ffmpeg")
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
                "-y",
            ])
            .arg(&audio)
            .status()?;
        assert!(status.success());
        let mut summary = BTreeMap::new();
        for entry in std::fs::read_dir(dir.path())? {
            let entry = entry?;
            summarize_file(&mut summary, &entry.path(), entry.metadata()?.len())?;
        }
        assert_eq!(
            summary["png"],
            MediaCount {
                count: 1,
                bytes: std::fs::metadata(image)?.len()
            }
        );
        assert_eq!(
            summary["vorbis"],
            MediaCount {
                count: 1,
                bytes: std::fs::metadata(audio)?.len()
            }
        );
        Ok(())
    }
    #[test]
    fn recognizes_content_and_animation() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let images = fixture.root().join("game/images");
        for (name, expected) in [
            ("opaque.png", ImageFormat::Png),
            ("animated.png", ImageFormat::AnimatedPng),
            ("café.jpg", ImageFormat::Jpeg),
            ("existing.webp", ImageFormat::Webp),
            ("mislabelled.png", ImageFormat::Unsupported),
            ("BG.PNG", ImageFormat::Png),
        ] {
            let path = images.join(name);
            assert_eq!(detect(&path)?, expected, "{name}");
            assert_eq!(detect_bytes(&std::fs::read(&path)?), expected, "{name}");
        }
        let mut animated = b"RIFF0000WEBPVP8X0000".to_vec();
        animated.push(2);
        assert_eq!(detect_bytes(&animated), ImageFormat::AnimatedWebp);
        assert_eq!(detect_bytes(b"not an image"), ImageFormat::Unsupported);
        Ok(())
    }
}
