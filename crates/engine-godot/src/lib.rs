//! Readers and writers for exported Godot packs.
pub mod adapter;
pub mod audio_transform;
pub mod ctex;
pub mod resource;
pub mod texture_transform;
pub mod vorbis;
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn u32le<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn u64le<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

#[derive(Clone, Debug)]
pub struct Header {
    pub format: u32,
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub flags: u32,
    pub file_base: u64,
    pub directory_offset: Option<u64>,
}
#[derive(Clone, Debug)]
pub struct Member {
    pub path: String,
    pub offset: u64,
    pub size: u64,
    pub md5: [u8; 16],
    pub flags: u32,
}
#[derive(Clone, Debug)]
pub struct Pack {
    pub start: u64,
    pub length: u64,
    pub header: Header,
    pub members: Vec<Member>,
}

/// Locate a standalone pack or an appended pack via its editor trailer.
pub fn locate_trailer(path: &Path) -> io::Result<Option<(u64, u64)>> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    if len < 4 {
        return Ok(None);
    }
    let mut magic = [0; 4];
    f.read_exact(&mut magic)?;
    if &magic == b"GDPC" {
        return Ok(Some((0, len)));
    }
    if len < 12 {
        return Ok(None);
    }
    f.seek(SeekFrom::End(-12))?;
    let size = u64le(&mut f)?;
    f.read_exact(&mut magic)?;
    if &magic != b"GDPC" {
        return Ok(None);
    }
    let start = len
        .checked_sub(12)
        .and_then(|x| x.checked_sub(size))
        .ok_or_else(|| invalid("invalid pack trailer"))?;
    f.seek(SeekFrom::Start(start))?;
    f.read_exact(&mut magic)?;
    if &magic != b"GDPC" {
        return Err(invalid("invalid pack trailer"));
    }
    Ok(Some((start, size)))
}

/// Find the raw offset of a PE section named `pck`.
pub fn locate_pe_section(path: &Path) -> io::Result<Option<u64>> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    if len < 64 {
        return Ok(None);
    }
    let mut mz = [0; 2];
    f.read_exact(&mut mz)?;
    if &mz != b"MZ" {
        return Ok(None);
    }
    f.seek(SeekFrom::Start(0x3c))?;
    let pe = u64::from(u32le(&mut f)?);
    if pe.checked_add(24).is_none_or(|end| end > len) {
        return Err(invalid("invalid PE header"));
    }
    f.seek(SeekFrom::Start(pe))?;
    let mut signature = [0; 4];
    f.read_exact(&mut signature)?;
    if &signature != b"PE\0\0" {
        return Err(invalid("invalid PE header"));
    }
    let mut coff = [0; 20];
    f.read_exact(&mut coff)?;
    let count = u16::from_le_bytes([coff[2], coff[3]]) as u64;
    let optional = u16::from_le_bytes([coff[16], coff[17]]) as u64;
    let table = pe + 24 + optional;
    if table.checked_add(count * 40).is_none_or(|end| end > len) {
        return Err(invalid("invalid PE sections"));
    }
    f.seek(SeekFrom::Start(table))?;
    for _ in 0..count {
        let mut section = [0; 40];
        f.read_exact(&mut section)?;
        if &section[..4] == b"pck\0" {
            let pos = u32::from_le_bytes(section[20..24].try_into().unwrap()) as u64;
            if pos >= len {
                return Err(invalid("invalid PE pack section"));
            }
            return Ok(Some(pos));
        }
    }
    Ok(None)
}

pub fn read_header(path: &Path, start: u64) -> io::Result<Header> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(start))?;
    let mut magic = [0; 4];
    f.read_exact(&mut magic)?;
    if &magic != b"GDPC" {
        return Err(invalid("invalid pack magic"));
    }
    let format = u32le(&mut f)?;
    let major = u32le(&mut f)?;
    let minor = u32le(&mut f)?;
    let patch = u32le(&mut f)?;
    let flags = u32le(&mut f)?;
    let file_base = u64le(&mut f)?;
    let directory_offset = if format >= 3 {
        Some(u64le(&mut f)?)
    } else {
        None
    };
    Ok(Header {
        format,
        major,
        minor,
        patch,
        flags,
        file_base,
        directory_offset,
    })
}

pub fn read_pack(path: &Path, start: u64, length: u64) -> io::Result<Pack> {
    let header = read_header(path, start)?;
    if !(2..=4).contains(&header.format) || header.major != 4 {
        return Err(invalid("unsupported pack format"));
    }
    if header.flags & 1 != 0 {
        return Err(invalid("encrypted pack"));
    }
    if header.flags & 4 != 0 {
        return Err(invalid("sparse bundle pack"));
    }
    let mut f = File::open(path)?;
    let end = start
        .checked_add(length)
        .filter(|end| *end <= f.metadata().map(|m| m.len()).unwrap_or(0))
        .ok_or_else(|| invalid("unreadable pack"))?;
    let directory = start
        .checked_add(header.directory_offset.unwrap_or(96))
        .filter(|v| *v < end)
        .ok_or_else(|| invalid("unreadable pack"))?;
    f.seek(SeekFrom::Start(directory))?;
    let count = u32le(&mut f)?;
    if u64::from(count) > length / 40 {
        return Err(invalid("unreadable pack"));
    }
    let mut members = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let n = u32le(&mut f)? as usize;
        if n == 0
            || n > 1024 * 1024
            || !n.is_multiple_of(4)
            || f.stream_position()?
                .checked_add(n as u64 + 36)
                .is_none_or(|v| v > end)
        {
            return Err(invalid("unreadable pack"));
        }
        let mut name = vec![0; n];
        f.read_exact(&mut name)?;
        let trimmed = name.iter().position(|b| *b == 0).unwrap_or(n);
        let name = std::str::from_utf8(&name[..trimmed])
            .map_err(|_| invalid("unreadable pack"))?
            .to_owned();
        let offset = u64le(&mut f)?;
        let size = u64le(&mut f)?;
        let mut md5 = [0; 16];
        f.read_exact(&mut md5)?;
        let flags = u32le(&mut f)?;
        // Format 2 without PACK_REL_FILEBASE stores an absolute file base.
        let base = if header.format == 2 && header.flags & 2 == 0 {
            Some(header.file_base)
        } else {
            start.checked_add(header.file_base)
        };
        let absolute = base
            .and_then(|v| v.checked_add(offset))
            .ok_or_else(|| invalid("unreadable pack"))?;
        if absolute.checked_add(size).is_none_or(|v| v > end) {
            return Err(invalid("unreadable pack"));
        }
        members.push(Member {
            path: name,
            offset: absolute,
            size,
            md5,
            flags,
        });
    }
    Ok(Pack {
        start,
        length,
        header,
        members,
    })
}

pub fn read_member(path: &Path, member: &Member) -> io::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(member.offset))?;
    let n = usize::try_from(member.size).map_err(|_| invalid("member too large"))?;
    let mut data = vec![0; n];
    f.read_exact(&mut data)?;
    Ok(data)
}

/// Rebuild a standalone pack and verify every member before returning.
pub fn rebuild_pack(
    input: &Path,
    output: &Path,
    replacements: &std::collections::BTreeMap<String, std::path::PathBuf>,
) -> io::Result<()> {
    use md5::{Digest, Md5};
    use std::io::Write;
    let (start, len) = locate_trailer(input)?.ok_or_else(|| invalid("unreadable pack"))?;
    if start != 0 {
        return Err(invalid("embedded pack requires executable rewriting"));
    }
    let pack = read_pack(input, start, len)?;
    if replacements
        .keys()
        .any(|name| !pack.members.iter().any(|member| &member.path == name))
    {
        return Err(invalid("replacement member missing"));
    }
    if replacements.is_empty() {
        std::fs::copy(input, output)?;
        let copied = read_pack(output, 0, std::fs::metadata(output)?.len())?;
        for (before, after) in pack.members.iter().zip(&copied.members) {
            if before.path != after.path
                || blake3::hash(&read_member(input, before)?)
                    != blake3::hash(&read_member(output, after)?)
            {
                return Err(invalid("pack self-check failed"));
            }
        }
        return Ok(());
    }
    let mut input_file = File::open(input)?;
    let mut alignment = 32u64;
    while alignment > 1
        && (pack.header.file_base % alignment != 0
            || pack
                .members
                .iter()
                .any(|m| (m.offset - start) % alignment != 0))
    {
        alignment /= 2;
    }
    let base = pack.header.file_base;
    let mut prefix =
        vec![0u8; usize::try_from(base).map_err(|_| invalid("pack header too large"))?];
    input_file.read_exact(&mut prefix)?;
    let mut out = File::create(output)?;
    out.write_all(&prefix)?;
    let mut members = pack.members.clone();
    let mut order: Vec<usize> = (0..members.len()).collect();
    order.sort_by_key(|i| members[*i].offset);
    let mut hashes = Vec::with_capacity(members.len());
    hashes.resize(members.len(), blake3::Hash::from([0u8; 32]));
    for i in order {
        let current = out.stream_position()?;
        let aligned = current.div_ceil(alignment) * alignment;
        if aligned > current {
            out.write_all(&vec![0; (aligned - current) as usize])?;
        }
        members[i].offset = aligned;
        let data = if let Some(path) = replacements.get(&members[i].path) {
            let bytes = std::fs::read(path)?;
            members[i].md5 = Md5::digest(&bytes).into();
            bytes
        } else {
            read_member(input, &pack.members[i])?
        };
        members[i].size = data.len() as u64;
        hashes[i] = blake3::hash(&data);
        out.write_all(&data)?;
    }
    let current = out.stream_position()?;
    let aligned = current.div_ceil(alignment) * alignment;
    if aligned > current {
        out.write_all(&vec![0; (aligned - current) as usize])?;
    }
    let directory = if pack.header.format == 2 { 96 } else { aligned };
    out.seek(SeekFrom::Start(directory))?;
    out.write_all(&(members.len() as u32).to_le_bytes())?;
    for member in &members {
        let name = member.path.as_bytes();
        let n = (name.len() + 3) & !3;
        out.write_all(&(n as u32).to_le_bytes())?;
        out.write_all(name)?;
        out.write_all(&vec![0; n - name.len()])?;
        out.write_all(&(member.offset - base).to_le_bytes())?;
        out.write_all(&member.size.to_le_bytes())?;
        out.write_all(&member.md5)?;
        out.write_all(&member.flags.to_le_bytes())?;
    }
    if pack.header.format >= 3 {
        out.seek(SeekFrom::Start(32))?;
        out.write_all(&directory.to_le_bytes())?;
    }
    out.flush()?;
    drop(out);
    let rebuilt = read_pack(output, 0, std::fs::metadata(output)?.len())?;
    if rebuilt.members.len() != members.len() {
        return Err(invalid("pack self-check failed"));
    }
    for (i, member) in rebuilt.members.iter().enumerate() {
        if member.path != members[i].path
            || blake3::hash(&read_member(output, member)?) != hashes[i]
        {
            return Err(invalid("pack self-check failed"));
        }
    }
    Ok(())
}

/// Return the PE section size field for an unsigned embedded-pack host.
pub fn validate_embedded_host(input: &Path, start: u64) -> io::Result<Option<u64>> {
    let mut source = File::open(input)?;
    let mut mz = [0; 2];
    source.read_exact(&mut mz)?;
    if &mz != b"MZ" {
        return Err(invalid("unsupported executable format"));
    }
    source.seek(SeekFrom::Start(0x3c))?;
    let pe = u64::from(u32le(&mut source)?);
    source.seek(SeekFrom::Start(pe))?;
    let mut sig = [0; 4];
    source.read_exact(&mut sig)?;
    if &sig != b"PE\0\0" {
        return Err(invalid("unsupported executable format"));
    }
    let mut coff = [0; 20];
    source.read_exact(&mut coff)?;
    let sections = u16::from_le_bytes([coff[2], coff[3]]) as u64;
    let optional_size = u16::from_le_bytes([coff[16], coff[17]]) as u64;
    let optional = pe + 24;
    source.seek(SeekFrom::Start(optional))?;
    let mut m = [0; 2];
    source.read_exact(&mut m)?;
    let security_offset = match u16::from_le_bytes(m) {
        0x10b => 96 + 8 * 4,
        0x20b => 112 + 8 * 4,
        _ => return Err(invalid("unsupported executable format")),
    };
    if optional_size < security_offset + 8 || optional + optional_size + sections * 40 > start {
        return Err(invalid("unsupported executable format"));
    }
    source.seek(SeekFrom::Start(optional + security_offset))?;
    let cert_offset = u32le(&mut source)?;
    let cert_size = u32le(&mut source)?;
    if cert_offset != 0 || cert_size != 0 {
        return Err(invalid("signed executable"));
    }
    let section_table = optional + optional_size;
    source.seek(SeekFrom::Start(section_table))?;
    let mut section_size_offset = None;
    for i in 0..sections {
        let mut section = [0; 40];
        source.read_exact(&mut section)?;
        if u64::from(u32::from_le_bytes(section[20..24].try_into().unwrap())) == start {
            section_size_offset = Some(section_table + i * 40 + 16);
        }
    }
    Ok(section_size_offset)
}

/// Rewrite an unsigned PE executable containing an appended pack.
pub fn rebuild_embedded(
    input: &Path,
    output: &Path,
    replacements: &std::collections::BTreeMap<String, std::path::PathBuf>,
) -> io::Result<()> {
    use std::io::Write;
    let (start, length) = locate_trailer(input)?.ok_or_else(|| invalid("unreadable pack"))?;
    if start == 0 {
        return Err(invalid("unsupported executable format"));
    }
    let section_size_offset = validate_embedded_host(input, start)?;
    let mut source = File::open(input)?;
    let source_pack = tempfile::NamedTempFile::new()?;
    let built_pack = tempfile::NamedTempFile::new()?;
    source.seek(SeekFrom::Start(start))?;
    io::copy(&mut (&mut source).take(length), &mut source_pack.reopen()?)?;
    rebuild_pack(source_pack.path(), built_pack.path(), replacements)?;
    source.seek(SeekFrom::Start(0))?;
    let mut out = File::create(output)?;
    io::copy(&mut (&mut source).take(start), &mut out)?;
    io::copy(&mut File::open(built_pack.path())?, &mut out)?;
    let pack_end = out.stream_position()?;
    let padding = (8 - (pack_end - start) % 8) % 8;
    out.write_all(&vec![0; padding as usize])?;
    let pack_size = out.stream_position()? - start;
    out.write_all(&pack_size.to_le_bytes())?;
    out.write_all(b"GDPC")?;
    let tail = out.stream_position()? - start;
    if let Some(offset) = section_size_offset {
        out.seek(SeekFrom::Start(offset))?;
        out.write_all(
            &u32::try_from(tail)
                .map_err(|_| invalid("pack section too large"))?
                .to_le_bytes(),
        )?;
    }
    out.flush()?;
    drop(out);
    let (found, rebuilt_len) =
        locate_trailer(output)?.ok_or_else(|| invalid("pack self-check failed"))?;
    if found != start
        || (section_size_offset.is_some() && locate_pe_section(output)? != Some(start))
    {
        return Err(invalid("pack self-check failed"));
    }
    let rebuilt = read_pack(output, found, rebuilt_len)?;
    let expected = read_pack(
        built_pack.path(),
        0,
        std::fs::metadata(built_pack.path())?.len(),
    )?;
    if rebuilt.members.len() != expected.members.len() {
        return Err(invalid("pack self-check failed"));
    }
    for (a, b) in rebuilt.members.iter().zip(&expected.members) {
        if a.path != b.path
            || blake3::hash(&read_member(output, a)?)
                != blake3::hash(&read_member(built_pack.path(), b)?)
        {
            return Err(invalid("pack self-check failed"));
        }
    }
    Ok(())
}

pub struct GodotPackFormat;
impl compressor_core::registry::PackFormat for GodotPackFormat {
    fn read_directory(
        &self,
        input: &Path,
    ) -> io::Result<Vec<compressor_core::registry::PackMember>> {
        let (start, len) = locate_trailer(input)?.ok_or_else(|| invalid("unreadable pack"))?;
        Ok(read_pack(input, start, len)?
            .members
            .into_iter()
            .map(|m| compressor_core::registry::PackMember {
                name: m.path,
                offset: m.offset,
                size: m.size,
            })
            .collect())
    }
    fn read_member(
        &self,
        input: &Path,
        member: &compressor_core::registry::PackMember,
    ) -> io::Result<Vec<u8>> {
        read_member(
            input,
            &Member {
                path: member.name.clone(),
                offset: member.offset,
                size: member.size,
                md5: [0; 16],
                flags: 0,
            },
        )
    }
    fn rebuild(
        &self,
        input: &Path,
        output: &Path,
        replacements: &std::collections::BTreeMap<String, std::path::PathBuf>,
    ) -> io::Result<()> {
        let (start, _) = locate_trailer(input)?.ok_or_else(|| invalid("unreadable pack"))?;
        if start == 0 {
            rebuild_pack(input, output, replacements)
        } else {
            rebuild_embedded(input, output, replacements)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_synthetic_formats_and_trailer() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        for format in 2..=4 {
            let pack = compressor_test_fixtures::godot_pack(format, 2);
            let path = dir.path().join(format!("{format}.pck"));
            std::fs::write(&path, &pack)?;
            let (start, len) = locate_trailer(&path)?.unwrap();
            let parsed = read_pack(&path, start, len)?;
            assert_eq!(parsed.header.format, format);
            assert_eq!(parsed.members.len(), 2);
            assert_eq!(parsed.members[0].path, "a");
            assert_eq!(read_member(&path, &parsed.members[1])?, b"two!");
            let embedded = dir.path().join(format!("{format}.exe"));
            let mut bytes = vec![42; 64];
            bytes.extend(&pack);
            bytes.extend((pack.len() as u64).to_le_bytes());
            bytes.extend(b"GDPC");
            std::fs::write(&embedded, bytes)?;
            assert_eq!(locate_trailer(&embedded)?, Some((64, pack.len() as u64)));
            assert_eq!(
                read_pack(&embedded, 64, pack.len() as u64)?.members.len(),
                2
            );
        }
        // Format 2 without PACK_REL_FILEBASE: the file base is absolute in the host file.
        let mut pack = compressor_test_fixtures::godot_pack(2, 0);
        let base = u64::from_le_bytes(pack[24..32].try_into().unwrap());
        pack[24..32].copy_from_slice(&(base + 64).to_le_bytes());
        let embedded = dir.path().join("absolute.exe");
        let mut bytes = vec![42; 64];
        bytes.extend(&pack);
        bytes.extend((pack.len() as u64).to_le_bytes());
        bytes.extend(b"GDPC");
        std::fs::write(&embedded, bytes)?;
        let parsed = read_pack(&embedded, 64, pack.len() as u64)?;
        assert_eq!(read_member(&embedded, &parsed.members[1])?, b"two!");
        Ok(())
    }
    #[test]
    fn rebuilds_formats_and_preserves_other_members() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        for format in 2..=4 {
            let source = dir.path().join(format!("source{format}"));
            let output = dir.path().join(format!("output{format}"));
            let replacement = dir.path().join("replacement");
            let original = compressor_test_fixtures::godot_pack(format, 2);
            std::fs::write(&source, &original)?;
            rebuild_pack(&source, &output, &Default::default())?;
            assert_eq!(std::fs::read(&output)?, original);
            std::fs::write(&replacement, b"new payload")?;
            rebuild_pack(
                &source,
                &output,
                &std::collections::BTreeMap::from([("a".into(), replacement)]),
            )?;
            let rebuilt = read_pack(&output, 0, std::fs::metadata(&output)?.len())?;
            assert_eq!(read_member(&output, &rebuilt.members[0])?, b"new payload");
            assert_eq!(read_member(&output, &rebuilt.members[1])?, b"two!");
            assert_eq!(rebuilt.header.format, format);
            assert_eq!(
                rebuilt.members[0].md5,
                [
                    0xd6, 0x8b, 0x30, 0x66, 0x1c, 0xd9, 0xfc, 0x58, 0xb4, 0xdc, 0xa0, 0x31, 0x66,
                    0xa1, 0x04, 0xd6
                ]
            );
        }
        Ok(())
    }
    fn synthetic_executable(signed: bool) -> Vec<u8> {
        let mut exe = vec![0; 512];
        exe[..2].copy_from_slice(b"MZ");
        exe[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        exe[0x80..0x84].copy_from_slice(b"PE\0\0");
        exe[0x86..0x88].copy_from_slice(&1u16.to_le_bytes());
        exe[0x94..0x96].copy_from_slice(&240u16.to_le_bytes());
        exe[0x98..0x9a].copy_from_slice(&0x20bu16.to_le_bytes());
        if signed {
            exe[0x128..0x12c].copy_from_slice(&1u32.to_le_bytes());
        }
        exe[0x188..0x18c].copy_from_slice(b"pck\0");
        exe[0x19c..0x1a0].copy_from_slice(&512u32.to_le_bytes());
        let pack = compressor_test_fixtures::godot_pack(4, 2);
        exe.extend(&pack);
        exe.extend((pack.len() as u64).to_le_bytes());
        exe.extend(b"GDPC");
        exe
    }
    #[test]
    fn embedded_rebuild_and_refusals() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("in.exe");
        let output = dir.path().join("out.exe");
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, b"replacement")?;
        let map = std::collections::BTreeMap::from([("a".into(), replacement)]);
        std::fs::write(&input, synthetic_executable(false))?;
        rebuild_embedded(&input, &output, &map)?;
        let (start, len) = locate_trailer(&output)?.unwrap();
        assert_eq!(start, 512);
        assert_eq!(locate_pe_section(&output)?, Some(start));
        let pack = read_pack(&output, start, len)?;
        assert_eq!(read_member(&output, &pack.members[0])?, b"replacement");
        let bytes = std::fs::read(&output)?;
        assert_eq!(
            u32::from_le_bytes(bytes[0x198..0x19c].try_into().unwrap()) as usize,
            bytes.len() - 512
        );
        std::fs::write(&input, synthetic_executable(true))?;
        assert_eq!(
            rebuild_embedded(&input, &output, &map)
                .unwrap_err()
                .to_string(),
            "signed executable"
        );
        let mut elf = synthetic_executable(false);
        elf[..4].copy_from_slice(b"\x7fELF");
        std::fs::write(&input, elf)?;
        assert_eq!(
            rebuild_embedded(&input, &output, &map)
                .unwrap_err()
                .to_string(),
            "unsupported executable format"
        );
        Ok(())
    }
    #[test]
    #[ignore = "requires example games"]
    fn rebuilds_real_embedded_pack() -> io::Result<()> {
        let input = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../example_games/GODOT_2_Bastion2 v4a - Win/Betrayal at the Bastion.exe");
        let (start, length) = locate_trailer(&input)?.unwrap();
        let pack = read_pack(&input, start, length)?;
        let dir = tempfile::tempdir()?;
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, b"replacement")?;
        let output = dir.path().join("out.exe");
        rebuild_embedded(
            &input,
            &output,
            &std::collections::BTreeMap::from([(pack.members[0].path.clone(), replacement)]),
        )?;
        let (new_start, len) = locate_trailer(&output)?.unwrap();
        assert_eq!(new_start, start);
        assert_eq!(locate_pe_section(&output)?, Some(start));
        assert_eq!(
            read_member(&output, &read_pack(&output, start, len)?.members[0])?,
            b"replacement"
        );
        Ok(())
    }
    #[test]
    fn finds_synthetic_pe_section() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("game.exe");
        let mut bytes = vec![0; 512];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
        bytes[0x86..0x88].copy_from_slice(&1u16.to_le_bytes());
        bytes[0x98..0x9c].copy_from_slice(b"pck\0");
        bytes[0xac..0xb0].copy_from_slice(&256u32.to_le_bytes());
        std::fs::write(&path, bytes)?;
        assert_eq!(locate_pe_section(&path)?, Some(256));
        Ok(())
    }
    #[test]
    fn rejects_unsupported_headers() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("game.pck");
        for (format, flags, reason) in [
            (4, 1, "encrypted pack"),
            (4, 4, "sparse bundle pack"),
            (5, 0, "unsupported pack format"),
        ] {
            let pack = compressor_test_fixtures::godot_pack(format, flags);
            std::fs::write(&path, &pack)?;
            assert_eq!(
                read_pack(&path, 0, pack.len() as u64)
                    .unwrap_err()
                    .to_string(),
                reason
            );
        }
        Ok(())
    }
    #[test]
    #[ignore = "requires example games"]
    fn reads_real_packs() -> io::Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../example_games");
        let one = root.join("GODOT_1_Premature Ejaculator and a Succubus/STST.pck");
        let two = root.join("GODOT_2_Bastion2 v4a - Win/Betrayal at the Bastion.exe");
        for (path, count, expected_start) in [(one, 1375, 0), (two, 3599, 109268480)] {
            let (start, len) = locate_trailer(&path)?.unwrap();
            assert_eq!(start, expected_start);
            if start != 0 {
                assert_eq!(locate_pe_section(&path)?, Some(start));
            }
            assert_eq!(read_pack(&path, start, len)?.members.len(), count);
        }
        Ok(())
    }
}
