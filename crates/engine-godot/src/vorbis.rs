//! Ogg page conversion for Godot's Vorbis packet sequence.
use std::io;
fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid Vorbis packet sequence")
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sequence {
    pub pages: Vec<Vec<Vec<u8>>>,
    pub granules: Vec<i64>,
    pub sample_rate: u32,
}
fn crc(bytes: &[u8]) -> u32 {
    let mut crc = 0u32;
    for byte in bytes {
        crc ^= u32::from(*byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c11db7
            } else {
                crc << 1
            };
        }
    }
    crc
}
impl Sequence {
    pub fn from_resource(resource: &crate::resource::Resource) -> io::Result<Self> {
        let (i, _) = resource
            .bodies
            .iter()
            .enumerate()
            .find(|(_, b)| b.class == "OggPacketSequence")
            .ok_or_else(bad)?;
        let pages =
            crate::resource::parse_packets(resource.property(i, "packet_data").ok_or_else(bad)?)?;
        let granules = crate::resource::parse_int64(
            resource.property(i, "granule_positions").ok_or_else(bad)?,
        )?;
        let first = pages.first().and_then(|p| p.first()).ok_or_else(bad)?;
        if first.len() < 16 || &first[..7] != b"\x01vorbis" {
            return Err(bad());
        }
        let sample_rate = u32::from_le_bytes(first[12..16].try_into().unwrap());
        if sample_rate == 0 || pages.len() != granules.len() {
            return Err(bad());
        }
        Ok(Self {
            pages,
            granules,
            sample_rate,
        })
    }
    pub fn to_ogg(&self) -> io::Result<Vec<u8>> {
        if self.pages.len() != self.granules.len() || self.pages.is_empty() {
            return Err(bad());
        }
        let mut out = Vec::new();
        let mut seq = 0u32;
        for (page_index, packets) in self.pages.iter().enumerate() {
            let mut laces = Vec::new();
            let mut data = Vec::new();
            for packet in packets {
                let mut left = packet.len();
                let mut pos = 0;
                while left >= 255 {
                    laces.push(255u8);
                    data.extend(&packet[pos..pos + 255]);
                    pos += 255;
                    left -= 255;
                }
                laces.push(left as u8);
                data.extend(&packet[pos..]);
            }
            if laces.is_empty() {
                return Err(bad());
            }
            let mut lpos = 0;
            let mut dpos = 0;
            let mut continued = false;
            while lpos < laces.len() {
                let end = (lpos + 255).min(laces.len());
                let part = &laces[lpos..end];
                let bytes: usize = part.iter().map(|v| usize::from(*v)).sum();
                let final_part = end == laces.len();
                let mut header = vec![0u8; 27];
                header[..4].copy_from_slice(b"OggS");
                header[5] = u8::from(continued)
                    | if seq == 0 { 2 } else { 0 }
                    | if page_index + 1 == self.pages.len() && final_part {
                        4
                    } else {
                        0
                    };
                let granule = if final_part {
                    self.granules[page_index]
                } else {
                    -1
                };
                header[6..14].copy_from_slice(&granule.to_le_bytes());
                header[14..18].copy_from_slice(&1u32.to_le_bytes());
                header[18..22].copy_from_slice(&seq.to_le_bytes());
                header[26] = part.len() as u8;
                let start = out.len();
                out.extend(header);
                out.extend(part);
                out.extend(&data[dpos..dpos + bytes]);
                let checksum = crc(&out[start..]);
                out[start + 22..start + 26].copy_from_slice(&checksum.to_le_bytes());
                continued = part.last() == Some(&255);
                lpos = end;
                dpos += bytes;
                seq += 1;
            }
        }
        Ok(out)
    }
    pub fn from_ogg(bytes: &[u8]) -> io::Result<Self> {
        let mut pos = 0;
        let mut packet = Vec::new();
        let mut pages = Vec::new();
        let mut granules = Vec::new();
        let mut serial = None;
        let mut sequence = 0u32;
        while pos < bytes.len() {
            if pos + 27 > bytes.len() || &bytes[pos..pos + 4] != b"OggS" || bytes[pos + 4] != 0 {
                return Err(bad());
            }
            let count = bytes[pos + 26] as usize;
            if pos + 27 + count > bytes.len() {
                return Err(bad());
            }
            let laces = &bytes[pos + 27..pos + 27 + count];
            let n: usize = laces.iter().map(|n| usize::from(*n)).sum();
            let end = pos
                .checked_add(27 + count + n)
                .filter(|end| *end <= bytes.len())
                .ok_or_else(bad)?;
            let mut check = bytes[pos..end].to_vec();
            let expected = u32::from_le_bytes(check[22..26].try_into().unwrap());
            check[22..26].fill(0);
            if crc(&check) != expected {
                return Err(bad());
            }
            let id = u32::from_le_bytes(bytes[pos + 14..pos + 18].try_into().unwrap());
            if serial.get_or_insert(id) != &id
                || u32::from_le_bytes(bytes[pos + 18..pos + 22].try_into().unwrap()) != sequence
            {
                return Err(bad());
            }
            sequence += 1;
            if (bytes[pos + 5] & 1 != 0) != !packet.is_empty() {
                return Err(bad());
            }
            let mut cursor = pos + 27 + count;
            let mut page = Vec::new();
            for lace in laces {
                let next = cursor + usize::from(*lace);
                packet.extend(&bytes[cursor..next]);
                cursor = next;
                if *lace < 255 && !packet.is_empty() {
                    page.push(std::mem::take(&mut packet));
                }
            }
            if !page.is_empty() {
                pages.push(page);
                granules.push(i64::from_le_bytes(
                    bytes[pos + 6..pos + 14].try_into().unwrap(),
                ));
            }
            pos = end;
        }
        if !packet.is_empty() {
            return Err(bad());
        }
        let first = pages.first().and_then(|p| p.first()).ok_or_else(bad)?;
        if first.len() < 16 || &first[..7] != b"\x01vorbis" {
            return Err(bad());
        }
        let sample_rate = u32::from_le_bytes(first[12..16].try_into().unwrap());
        if sample_rate == 0 {
            return Err(bad());
        }
        Ok(Self {
            pages,
            granules,
            sample_rate,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn continuation_roundtrip() -> io::Result<()> {
        let mut id = vec![0; 30];
        id[..7].copy_from_slice(b"\x01vorbis");
        id[12..16].copy_from_slice(&44100u32.to_le_bytes());
        let sequence = Sequence {
            pages: vec![vec![id], vec![vec![42; 70000]]],
            granules: vec![0, 44100],
            sample_rate: 44100,
        };
        assert_eq!(Sequence::from_ogg(&sequence.to_ogg()?)?, sequence);
        Ok(())
    }
    #[tokio::test]
    #[ignore = "requires example games and ffmpeg"]
    async fn real_sequences_roundtrip_and_decode() -> io::Result<()> {
        use compressor_core::{
            registry::Transformer, AudioCodec, AudioEncode, AudioProps, ConvertSettings,
        };
        use tokio_util::sync::CancellationToken;
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../example_games/GODOT_1_Premature Ejaculator and a Succubus/STST.pck");
        let (start, len) = crate::locate_trailer(&path)?.unwrap();
        let pack = crate::read_pack(&path, start, len)?;
        let mut checked = false;
        for member in pack
            .members
            .iter()
            .filter(|m| m.path.ends_with(".oggvorbisstr"))
        {
            let res = crate::resource::Resource::parse(&crate::read_member(&path, member)?)?;
            let sequence = Sequence::from_resource(&res)?;
            let ogg = sequence.to_ogg()?;
            assert_eq!(Sequence::from_ogg(&ogg)?, sequence, "{}", member.path);
            let file = tempfile::NamedTempFile::new()?;
            let encoded = tempfile::NamedTempFile::new()?;
            std::fs::write(file.path(), &ogg)?;
            let first = &sequence.pages[0][0];
            let planned =
                *sequence.granules.last().unwrap() as f64 / f64::from(sequence.sample_rate);
            let settings = ConvertSettings::Audio(AudioEncode {
                codec: AudioCodec::Vorbis,
                bitrate_kbps: None,
                vorbis_quality: Some(3),
                source: AudioProps {
                    channels: u32::from(first[11]),
                    sample_rate: sequence.sample_rate,
                    duration_ms: (planned * 1000.0) as u64,
                },
            });
            compressor_media::audio::AudioTransformer
                .transform(
                    file.path(),
                    encoded.path(),
                    &settings,
                    &CancellationToken::new(),
                )
                .await?;
            let result = std::process::Command::new("ffmpeg")
                .args(["-nostdin", "-v", "error", "-xerror", "-i"])
                .arg(encoded.path())
                .args(["-f", "null", "-"])
                .output()?;
            assert!(
                result.status.success() && result.stderr.is_empty() && result.stdout.is_empty(),
                "{}: {}",
                member.path,
                String::from_utf8_lossy(&result.stderr)
            );
            let probe = std::process::Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "format=duration",
                    "-of",
                    "default=nw=1:nk=1",
                ])
                .arg(file.path())
                .output()?;
            let seconds = std::str::from_utf8(&probe.stdout)
                .unwrap()
                .trim()
                .parse::<f64>()
                .unwrap();
            assert!((seconds - planned).abs() <= 0.001, "{}", member.path);
            checked = true;
        }
        assert!(checked);
        Ok(())
    }
}
