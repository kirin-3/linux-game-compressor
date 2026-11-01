//! Content-based image detection and WebP conversion.

pub mod webp;
use std::{
    fs::File,
    io::{self, Cursor, Read, Seek, SeekFrom},
    path::Path,
};

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

fn detect_reader(reader: &mut (impl Read + Seek)) -> io::Result<ImageFormat> {
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
            assert_eq!(detect(&images.join(name))?, expected, "{name}");
        }
        let mut animated = b"RIFF0000WEBPVP8X0000".to_vec();
        animated.push(2);
        assert_eq!(detect_bytes(&animated), ImageFormat::AnimatedWebp);
        assert_eq!(detect_bytes(b"not an image"), ImageFormat::Unsupported);
        Ok(())
    }
}
