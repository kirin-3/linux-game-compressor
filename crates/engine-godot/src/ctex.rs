//! Godot 4 compressed texture containers.
use std::io;
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "unsupported texture type")
}
fn u32at(bytes: &[u8], pos: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(pos..pos + 4)
            .ok_or_else(invalid)?
            .try_into()
            .unwrap(),
    ))
}
fn u16at(bytes: &[u8], pos: usize) -> io::Result<u16> {
    Ok(u16::from_le_bytes(
        bytes
            .get(pos..pos + 2)
            .ok_or_else(invalid)?
            .try_into()
            .unwrap(),
    ))
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Texture {
    pub header: [u8; 36],
    pub data_format: u32,
    pub width: u16,
    pub height: u16,
    pub mipmaps: u32,
    pub image_format: u32,
    pub images: Vec<Vec<u8>>,
}
impl Texture {
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        let header: [u8; 36] = bytes.get(..36).ok_or_else(invalid)?.try_into().unwrap();
        if &header[..4] != b"GST2" {
            return Err(invalid());
        }
        let data_format = u32at(bytes, 36)?;
        let width = u16at(bytes, 40)?;
        let height = u16at(bytes, 42)?;
        let mipmaps = u32at(bytes, 44)?;
        let image_format = u32at(bytes, 48)?;
        let mut images = Vec::new();
        let mut pos = 52usize;
        if data_format == 1 || data_format == 2 {
            if mipmaps > 64 {
                return Err(invalid());
            }
            for _ in 0..=mipmaps {
                let size = u32at(bytes, pos)? as usize;
                pos += 4;
                let end = pos
                    .checked_add(size)
                    .filter(|end| *end <= bytes.len())
                    .ok_or_else(invalid)?;
                images.push(bytes[pos..end].to_vec());
                pos = end;
            }
        } else if data_format > 3 {
            return Err(invalid());
        }
        Ok(Self {
            header,
            data_format,
            width,
            height,
            mipmaps,
            image_format,
            images,
        })
    }
    pub fn webp_kind(&self) -> Option<&'static str> {
        let image = self.images.first()?;
        if image.len() < 16 || &image[..4] != b"RIFF" || &image[8..12] != b"WEBP" {
            return None;
        }
        match &image[12..16] {
            b"VP8L" => Some("lossless"),
            b"VP8 " => Some("lossy"),
            b"VP8X" => {
                // Extended WebP contains an image chunk after its feature header.
                let mut pos = 12usize;
                while pos + 8 <= image.len() {
                    let tag = &image[pos..pos + 4];
                    if tag == b"VP8L" {
                        return Some("lossless");
                    }
                    if tag == b"VP8 " {
                        return Some("lossy");
                    }
                    let len =
                        u32::from_le_bytes(image[pos + 4..pos + 8].try_into().unwrap()) as usize;
                    pos = pos.checked_add(8)?.checked_add(len)?.checked_add(len % 2)?;
                }
                None
            }
            _ => None,
        }
    }
    pub fn write(&self) -> io::Result<Vec<u8>> {
        if (self.data_format == 1 || self.data_format == 2)
            && self.images.len() != self.mipmaps as usize + 1
        {
            return Err(invalid());
        }
        if self.data_format > 3 || (self.data_format != 1 && self.data_format != 2) {
            return Err(invalid());
        }
        let mut out = self.header.to_vec();
        out.extend(self.data_format.to_le_bytes());
        out.extend(self.width.to_le_bytes());
        out.extend(self.height.to_le_bytes());
        out.extend(self.mipmaps.to_le_bytes());
        out.extend(self.image_format.to_le_bytes());
        for image in &self.images {
            out.extend(
                u32::try_from(image.len())
                    .map_err(|_| invalid())?
                    .to_le_bytes(),
            );
            out.extend(image);
        }
        Ok(out)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn texture(format: u32, mipmaps: u32, images: Vec<Vec<u8>>) -> Texture {
        let mut header = [0; 36];
        header[..4].copy_from_slice(b"GST2");
        Texture {
            header,
            data_format: format,
            width: 16,
            height: 8,
            mipmaps,
            image_format: 5,
            images,
        }
    }
    #[test]
    fn parses_encoded_and_raw_textures() -> io::Result<()> {
        let webp = b"RIFFxxxxWEBPVP8Ldata".to_vec();
        for t in [
            texture(2, 0, vec![webp.clone()]),
            texture(1, 0, vec![b"PNGdata".to_vec()]),
            texture(2, 1, vec![webp.clone(), webp]),
        ] {
            let parsed = Texture::parse(&t.write()?)?;
            assert_eq!(parsed, t);
            if t.data_format == 2 {
                assert_eq!(parsed.webp_kind(), Some("lossless"));
            }
        }
        let mut raw = texture(1, 0, vec![vec![1]]).write()?;
        raw[36..40].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(Texture::parse(&raw)?.data_format, 0);
        raw[36..40].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(Texture::parse(&raw)?.data_format, 3);
        let mut lossy = texture(2, 0, vec![b"RIFFxxxxWEBPVP8 data".to_vec()]);
        assert_eq!(lossy.webp_kind(), Some("lossy"));
        lossy.images.clear();
        assert!(lossy.write().is_err());
        Ok(())
    }
    #[test]
    #[ignore = "requires example games"]
    fn parses_real_textures() -> io::Result<()> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../example_games/GODOT_1_Premature Ejaculator and a Succubus/STST.pck");
        let (start, len) = crate::locate_trailer(&path)?.unwrap();
        let pack = crate::read_pack(&path, start, len)?;
        let mut count = 0;
        for member in pack.members.iter().filter(|m| m.path.ends_with(".ctex")) {
            Texture::parse(&crate::read_member(&path, member)?)?;
            count += 1;
        }
        assert_eq!(count, 424);
        Ok(())
    }
}
