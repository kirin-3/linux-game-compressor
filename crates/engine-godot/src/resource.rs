//! Span-preserving reader for Godot 4 binary resources.
use std::{collections::BTreeMap, io, ops::Range};
fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "unsupported resource layout")
}
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(bad)?;
        let v = &self.data[self.pos..end];
        self.pos = end;
        Ok(v)
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string(&mut self) -> io::Result<String> {
        let n = self.u32()? as usize;
        let data = self.take(n)?;
        let data = data.strip_suffix(&[0]).unwrap_or(data);
        Ok(std::str::from_utf8(data).map_err(|_| bad())?.to_owned())
    }
    fn name(&mut self, strings: &[String]) -> io::Result<String> {
        let id = self.u32()?;
        if id & 0x8000_0000 != 0 {
            let data = self.take((id & 0x7fff_ffff) as usize)?;
            Ok(std::str::from_utf8(data).map_err(|_| bad())?.to_owned())
        } else {
            strings.get(id as usize).cloned().ok_or_else(bad)
        }
    }
    fn variant(&mut self, depth: usize) -> io::Result<()> {
        if depth > 64 {
            return Err(bad());
        }
        let tag = self.u32()?;
        match tag {
            1 | 42 | 43 => {}
            2 | 3 | 4 | 23 => {
                self.take(4)?;
            }
            40 | 41 => {
                self.take(8)?;
            }
            5 | 44 => {
                self.string()?;
            }
            10 | 45 => {
                self.take(8)?;
            }
            11 | 13 | 14 | 20 | 46 | 50 | 51 => {
                self.take(16)?;
            }
            12 | 47 => {
                self.take(12)?;
            }
            15 | 18 => {
                self.take(24)?;
            }
            16 => {
                self.take(36)?;
            }
            17 => {
                self.take(48)?;
            }
            52 => {
                self.take(64)?;
            }
            22 => {
                let a = u16::from_le_bytes(self.take(2)?.try_into().unwrap()) as usize;
                let b = (u16::from_le_bytes(self.take(2)?.try_into().unwrap()) & 0x7fff) as usize;
                for _ in 0..a + b {
                    let id = self.u32()?;
                    if id & 0x8000_0000 != 0 {
                        self.take((id & 0x7fff_ffff) as usize)?;
                    }
                }
            }
            24 => match self.u32()? {
                0 => {}
                1 => {
                    self.string()?;
                    self.string()?;
                }
                2 | 3 => {
                    self.take(4)?;
                }
                _ => return Err(bad()),
            },
            26 | 30 => {
                let count = (self.u32()? & 0x7fff_ffff) as usize;
                let count = count
                    .checked_mul(if tag == 26 { 2 } else { 1 })
                    .ok_or_else(bad)?;
                if count > self.data.len() / 4 {
                    return Err(bad());
                }
                for _ in 0..count {
                    self.variant(depth + 1)?;
                }
            }
            31 => {
                let n = self.u32()? as usize;
                self.take(n.checked_add((4 - n % 4) % 4).ok_or_else(bad)?)?;
            }
            32 | 33 | 35 | 36 | 37 | 48 | 49 | 53 => {
                let n = self.u32()? as usize;
                let size = match tag {
                    32 | 33 => 4,
                    48 | 49 => 8,
                    35 => 12,
                    36 | 53 => 16,
                    37 => 8,
                    _ => unreachable!(),
                };
                self.take(n.checked_mul(size).ok_or_else(bad)?)?;
            }
            34 => {
                let n = self.u32()? as usize;
                if n > self.data.len() / 4 {
                    return Err(bad());
                }
                for _ in 0..n {
                    self.string()?;
                }
            }
            _ => return Err(bad()),
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct Property {
    pub name: String,
    pub span: Range<usize>,
}
#[derive(Clone, Debug)]
pub struct Body {
    pub class: String,
    pub properties: Vec<Property>,
    pub offset: usize,
    offset_field: usize,
}
#[derive(Clone, Debug)]
pub struct Resource {
    bytes: Vec<u8>,
    pub class: String,
    pub bodies: Vec<Body>,
}
impl Resource {
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.starts_with(b"RSCC") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "compressed resource",
            ));
        }
        let mut r = Reader {
            data: bytes,
            pos: 0,
        };
        if r.take(4)? != b"RSRC" || r.u32()? != 0 || r.u32()? != 0 {
            return Err(bad());
        }
        let major = r.u32()?;
        r.u32()?;
        let version = r.u32()?;
        if major != 4 || version != 6 {
            return Err(bad());
        }
        let class = r.string()?;
        r.u64()?;
        let flags = r.u32()?;
        if flags & 4 != 0 {
            return Err(bad());
        }
        r.u64()?;
        if flags & 8 != 0 {
            r.string()?;
        }
        r.take(44)?;
        let count = r.u32()? as usize;
        if count > bytes.len() / 4 {
            return Err(bad());
        }
        let mut strings = Vec::new();
        for _ in 0..count {
            strings.push(r.string()?);
        }
        let count = r.u32()? as usize;
        if count > bytes.len() / 8 {
            return Err(bad());
        }
        for _ in 0..count {
            r.string()?;
            r.string()?;
            if flags & 2 != 0 {
                r.u64()?;
            }
        }
        let count = r.u32()? as usize;
        if count > bytes.len() / 12 {
            return Err(bad());
        }
        let mut indexes = Vec::new();
        for _ in 0..count {
            r.string()?;
            let field = r.pos;
            let offset = usize::try_from(r.u64()?).map_err(|_| bad())?;
            indexes.push((offset, field));
        }
        let mut bodies = Vec::new();
        for (offset, offset_field) in indexes {
            if offset >= bytes.len() {
                return Err(bad());
            }
            r.pos = offset;
            let class = r.string()?;
            let n = r.u32()? as usize;
            if n > bytes.len() / 8 {
                return Err(bad());
            }
            let mut properties = Vec::new();
            for _ in 0..n {
                let name = r.name(&strings)?;
                let start = r.pos;
                r.variant(0)?;
                properties.push(Property {
                    name,
                    span: start..r.pos,
                });
            }
            bodies.push(Body {
                class,
                properties,
                offset,
                offset_field,
            });
        }
        Ok(Self {
            bytes: bytes.to_vec(),
            class,
            bodies,
        })
    }
    pub fn property(&self, body: usize, name: &str) -> Option<&[u8]> {
        let p = self
            .bodies
            .get(body)?
            .properties
            .iter()
            .find(|p| p.name == name)?;
        Some(&self.bytes[p.span.clone()])
    }
    pub fn write(&self, replacements: &BTreeMap<(usize, String), Vec<u8>>) -> io::Result<Vec<u8>> {
        let mut edits = Vec::new();
        for ((body, name), value) in replacements {
            let p = self
                .bodies
                .get(*body)
                .and_then(|b| b.properties.iter().find(|p| &p.name == name))
                .ok_or_else(bad)?;
            let mut check = Reader {
                data: value,
                pos: 0,
            };
            check.variant(0)?;
            if check.pos != value.len() {
                return Err(bad());
            }
            edits.push((p.span.clone(), value));
        }
        edits.sort_by_key(|e| e.0.start);
        if edits.windows(2).any(|w| w[0].0.end > w[1].0.start) {
            return Err(bad());
        }
        let mut out = Vec::new();
        let mut pos = 0;
        for (span, value) in &edits {
            out.extend(&self.bytes[pos..span.start]);
            out.extend(*value);
            pos = span.end;
        }
        out.extend(&self.bytes[pos..]);
        for body in &self.bodies {
            let shift: i64 = edits
                .iter()
                .filter(|(span, _)| span.end <= body.offset)
                .map(|(span, val)| val.len() as i64 - (span.end - span.start) as i64)
                .sum();
            let new_offset = (body.offset as i64)
                .checked_add(shift)
                .and_then(|v| u64::try_from(v).ok())
                .ok_or_else(bad)?;
            out[body.offset_field..body.offset_field + 8]
                .copy_from_slice(&new_offset.to_le_bytes());
        }
        Ok(out)
    }
}
pub fn byte_array(bytes: &[u8]) -> io::Result<Vec<u8>> {
    let mut v = 31u32.to_le_bytes().to_vec();
    v.extend(u32::try_from(bytes.len()).map_err(|_| bad())?.to_le_bytes());
    v.extend(bytes);
    v.resize((v.len() + 3) & !3, 0);
    Ok(v)
}

pub fn packet_arrays(pages: &[Vec<Vec<u8>>]) -> io::Result<Vec<u8>> {
    fn count(out: &mut Vec<u8>, n: usize) -> io::Result<()> {
        out.extend(u32::try_from(n).map_err(|_| bad())?.to_le_bytes());
        Ok(())
    }
    let mut out = 30u32.to_le_bytes().to_vec();
    count(&mut out, pages.len())?;
    for page in pages {
        out.extend(30u32.to_le_bytes());
        count(&mut out, page.len())?;
        for packet in page {
            out.extend(byte_array(packet)?);
        }
    }
    Ok(out)
}
pub fn packed_int64(values: &[i64]) -> io::Result<Vec<u8>> {
    let mut out = 48u32.to_le_bytes().to_vec();
    out.extend(
        u32::try_from(values.len())
            .map_err(|_| bad())?
            .to_le_bytes(),
    );
    for value in values {
        out.extend(value.to_le_bytes());
    }
    Ok(out)
}
pub fn real(value: f64) -> Vec<u8> {
    if (value as f32) as f64 == value {
        let mut out = 4u32.to_le_bytes().to_vec();
        out.extend((value as f32).to_le_bytes());
        out
    } else {
        let mut out = 41u32.to_le_bytes().to_vec();
        out.extend(value.to_le_bytes());
        out
    }
}
pub fn parse_packets(bytes: &[u8]) -> io::Result<Vec<Vec<Vec<u8>>>> {
    let mut r = Reader {
        data: bytes,
        pos: 0,
    };
    if r.u32()? != 30 {
        return Err(bad());
    }
    let count = r.u32()? as usize;
    if count > bytes.len() / 8 {
        return Err(bad());
    }
    let mut pages = Vec::new();
    for _ in 0..count {
        if r.u32()? != 30 {
            return Err(bad());
        }
        let n = r.u32()? as usize;
        if n > bytes.len() / 8 {
            return Err(bad());
        }
        let mut packets = Vec::new();
        for _ in 0..n {
            if r.u32()? != 31 {
                return Err(bad());
            }
            let len = r.u32()? as usize;
            packets.push(r.take(len)?.to_vec());
            r.take((4 - len % 4) % 4)?;
        }
        pages.push(packets);
    }
    if r.pos != bytes.len() {
        return Err(bad());
    }
    Ok(pages)
}
pub fn parse_int64(bytes: &[u8]) -> io::Result<Vec<i64>> {
    let mut r = Reader {
        data: bytes,
        pos: 0,
    };
    if r.u32()? != 48 {
        return Err(bad());
    }
    let n = r.u32()? as usize;
    if n > (bytes.len() - 8) / 8 {
        return Err(bad());
    }
    let mut values = Vec::with_capacity(n);
    for _ in 0..n {
        values.push(i64::from_le_bytes(r.take(8)?.try_into().unwrap()));
    }
    if r.pos != bytes.len() {
        return Err(bad());
    }
    Ok(values)
}

pub fn parse_byte_array(bytes: &[u8]) -> io::Result<&[u8]> {
    let mut r = Reader {
        data: bytes,
        pos: 0,
    };
    if r.u32()? != 31 {
        return Err(bad());
    }
    let n = r.u32()? as usize;
    let value = r.take(n)?;
    r.take((4 - n % 4) % 4)?;
    if r.pos != bytes.len() {
        return Err(bad());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn put_string(out: &mut Vec<u8>, value: &str) {
        out.extend(((value.len() + 1) as u32).to_le_bytes());
        out.extend(value.as_bytes());
        out.push(0);
    }
    fn fixture() -> Vec<u8> {
        let mut data = b"RSRC".to_vec();
        for n in [0u32, 0, 4, 0, 6] {
            data.extend(n.to_le_bytes());
        }
        put_string(&mut data, "Main");
        data.extend(0u64.to_le_bytes());
        data.extend(0u32.to_le_bytes());
        data.extend(0u64.to_le_bytes());
        data.extend([0; 44]);
        data.extend(1u32.to_le_bytes());
        put_string(&mut data, "data");
        data.extend(0u32.to_le_bytes());
        data.extend(2u32.to_le_bytes());
        put_string(&mut data, "first");
        let a = data.len();
        data.extend(0u64.to_le_bytes());
        put_string(&mut data, "second");
        let b = data.len();
        data.extend(0u64.to_le_bytes());
        let first = data.len() as u64;
        put_string(&mut data, "First");
        data.extend(1u32.to_le_bytes());
        data.extend(0u32.to_le_bytes());
        data.extend(byte_array(b"abc").unwrap());
        let second = data.len() as u64;
        put_string(&mut data, "Second");
        data.extend(1u32.to_le_bytes());
        data.extend(0u32.to_le_bytes());
        data.extend(byte_array(b"xyz").unwrap());
        data[a..a + 8].copy_from_slice(&first.to_le_bytes());
        data[b..b + 8].copy_from_slice(&second.to_le_bytes());
        data
    }
    #[test]
    fn replaces_one_span_and_updates_following_offsets() -> io::Result<()> {
        let bytes = fixture();
        let original = Resource::parse(&bytes)?;
        assert_eq!(original.write(&BTreeMap::new())?, bytes);
        let encoded = byte_array(b"a much longer byte array")?;
        let changed = original.write(&BTreeMap::from([((0, "data".into()), encoded.clone())]))?;
        let parsed = Resource::parse(&changed)?;
        assert_eq!(parsed.property(0, "data"), Some(encoded.as_slice()));
        assert_eq!(parsed.property(1, "data"), original.property(1, "data"));
        assert_eq!(
            parsed.bodies[1].offset - original.bodies[1].offset,
            encoded.len() - byte_array(b"abc")?.len()
        );
        Ok(())
    }
    #[test]
    #[ignore = "requires example games"]
    fn roundtrips_real_audio_resources() -> io::Result<()> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../example_games");
        for file in [
            "GODOT_1_Premature Ejaculator and a Succubus/STST.pck",
            "GODOT_2_Bastion2 v4a - Win/Betrayal at the Bastion.exe",
        ] {
            let path = root.join(file);
            let (start, len) = crate::locate_trailer(&path)?.unwrap();
            let pack = crate::read_pack(&path, start, len)?;
            for member in pack.members.iter().filter(|m| {
                [".oggvorbisstr", ".mp3str", ".sample"]
                    .iter()
                    .any(|suffix| m.path.ends_with(suffix))
            }) {
                let bytes = crate::read_member(&path, member)?;
                let resource = Resource::parse(&bytes)
                    .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", member.path)))?;
                assert_eq!(resource.write(&BTreeMap::new())?, bytes, "{}", member.path);
            }
        }
        Ok(())
    }
}
