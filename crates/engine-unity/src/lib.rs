//! Unity standalone player support.
use compressor_core::{
    registry::{ArchiveFormat, ArchiveInput, ArchivePlan, EngineAdapter, PackMember},
    Action, EngineInfo, OptionKind, OptionSpec, ResolvedSettings, StoredPath,
};
use rayon::prelude::*;
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

pub struct UnityFs;
pub struct UnityAdapter {
    pub allow_unknown_version: bool,
}
pub fn archive_params(version: &str, allow_unknown: bool) -> Option<serde_json::Value> {
    let mut numbers: Vec<u32> = version
        .split(|c: char| !c.is_ascii_digit())
        .take(3)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if numbers.len() == 2 {
        numbers.push(0);
    }
    if numbers.len() != 3 {
        return None;
    }
    let (year, minor, patch) = (numbers[0], numbers[1], numbers[2]);
    let format = match (year, minor) {
        (2020, 1 | 2) | (2021, 1 | 2) => 7,
        (2020, 3) => {
            if patch < 34 {
                7
            } else {
                8
            }
        }
        (2021, 3) => {
            if patch < 2 {
                7
            } else {
                8
            }
        }
        (2022, 1) => {
            if patch == 0 {
                7
            } else {
                8
            }
        }
        (2022, 2 | 3) | (2023, 1 | 2) | (6000, _) => 8,
        (year, _) if year > 6000 && allow_unknown => 8,
        _ => return None,
    };
    Some(
        serde_json::json!({"format_version": format, "unity_revision": version, "block_padding": format == 8, "block_size": 131072, "lz4hc_level": 9}),
    )
}

pub fn data_folders(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut parents = vec![PathBuf::new()];
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            parents.push(PathBuf::from(entry.file_name()));
        }
    }
    let mut folders = Vec::new();
    for parent in parents {
        for entry in std::fs::read_dir(root.join(&parent))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(base) = name.strip_suffix("_Data") else {
                continue;
            };
            if base.is_empty()
                || !["exe", "x86_64", "x86"]
                    .iter()
                    .any(|ext| root.join(&parent).join(format!("{base}.{ext}")).is_file())
            {
                continue;
            }
            let relative = parent.join(name);
            if root.join(&relative).join("globalgamemanagers").is_file()
                || root.join(&relative).join("data.unity3d").is_file()
            {
                folders.push(relative);
            }
        }
    }
    folders.sort();
    Ok(folders)
}
fn gathered(name: &str) -> bool {
    name == "globalgamemanagers"
        || name == "globalgamemanagers.assets"
        || name
            .strip_prefix("level")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        || name.ends_with(".assets")
        || name.ends_with(".resS")
}
fn decision(
    root: &Path,
    folder: &Path,
    settings: &ResolvedSettings,
    allow: bool,
) -> Result<serde_json::Value, &'static str> {
    if root.join(folder).join("data.unity3d").exists() {
        return Err("already compressed build");
    }
    if settings
        .engine_options
        .get("archive_data")
        .and_then(|v| v.value.as_bool())
        == Some(false)
    {
        return Err("archiving disabled");
    }
    let header = read_player_header(&root.join(folder).join("globalgamemanagers"))
        .map_err(|_| "unreadable player data")?;
    archive_params(&header.version, allow).ok_or("unsupported Unity version")
}
fn natural(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    while !a.is_empty() && !b.is_empty() {
        if a[0].is_ascii_digit() && b[0].is_ascii_digit() {
            let al = a.iter().take_while(|c| c.is_ascii_digit()).count();
            let bl = b.iter().take_while(|c| c.is_ascii_digit()).count();
            let aa = &a[..al];
            let bb = &b[..bl];
            let ai = aa
                .iter()
                .position(|c| *c != b'0')
                .unwrap_or(aa.len().saturating_sub(1));
            let bi = bb
                .iter()
                .position(|c| *c != b'0')
                .unwrap_or(bb.len().saturating_sub(1));
            let order = aa[ai..]
                .len()
                .cmp(&bb[bi..].len())
                .then(aa[ai..].cmp(&bb[bi..]))
                .then(aa.len().cmp(&bb.len()));
            if !order.is_eq() {
                return order;
            }
            a = &a[al..];
            b = &b[bl..];
        } else {
            let order = a[0].cmp(&b[0]);
            if !order.is_eq() {
                return order;
            }
            a = &a[1..];
            b = &b[1..];
        }
    }
    a.len().cmp(&b.len())
}
fn member_order(name: &str) -> u8 {
    if name == "globalgamemanagers" {
        0
    } else if name == "globalgamemanagers.assets" {
        1
    } else if name.starts_with("level") && name[5..].bytes().all(|b| b.is_ascii_digit()) {
        2
    } else if name.ends_with(".resS") {
        4
    } else {
        3
    }
}
impl EngineAdapter for UnityAdapter {
    fn id(&self) -> &'static str {
        "unity"
    }
    fn options(&self) -> Vec<OptionSpec> {
        vec![OptionSpec {
            name: "archive_data".into(),
            kind: OptionKind::Bool,
            default: serde_json::json!(true),
        }]
    }
    fn detect(&self, root: &Path) -> io::Result<Option<EngineInfo>> {
        let folders = data_folders(root)?;
        if folders.is_empty() {
            return Ok(None);
        }
        let version = read_player_header(&root.join(&folders[0]).join("globalgamemanagers"))
            .ok()
            .map(|h| h.version);
        Ok(Some(EngineInfo {
            name: "Unity".into(),
            version,
            excluded_paths: vec![],
            notes: vec![],
            version_check_overridden: self.allow_unknown_version,
        }))
    }
    fn archives(
        &self,
        root: &Path,
        engine: &EngineInfo,
        settings: &ResolvedSettings,
    ) -> io::Result<Vec<ArchivePlan>> {
        let mut archives = Vec::new();
        for folder in data_folders(root)? {
            let Ok(params) = decision(root, &folder, settings, engine.version_check_overridden)
            else {
                continue;
            };
            let mut members = Vec::new();
            for entry in std::fs::read_dir(root.join(&folder))? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    if let Some(name) = entry.file_name().to_str() {
                        if gathered(name) {
                            members.push(folder.join(name));
                        }
                    }
                }
            }
            members.sort_by(|a, b| {
                let a = a.file_name().unwrap().to_str().unwrap();
                let b = b.file_name().unwrap().to_str().unwrap();
                member_order(a)
                    .cmp(&member_order(b))
                    .then_with(|| natural(a, b))
            });
            if !members.is_empty() {
                archives.push(ArchivePlan {
                    path: folder.join("data.unity3d"),
                    format: "unity-fs".into(),
                    params,
                    members,
                });
            }
        }
        Ok(archives)
    }
    fn classify(
        &self,
        root: &Path,
        path: &Path,
        engine: &EngineInfo,
        settings: &ResolvedSettings,
    ) -> io::Result<Action> {
        let Some(parent) = path.parent() else {
            return Ok(Action::Copy {
                reason: "other game file".into(),
            });
        };
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if gathered(name)
            && std::fs::symlink_metadata(root.join(path))?.is_file()
            && data_folders(root)?.iter().any(|p| p == parent)
        {
            return Ok(
                match decision(root, parent, settings, engine.version_check_overridden) {
                    Ok(_) => Action::Archive {
                        archive: StoredPath(parent.join("data.unity3d")),
                    },
                    Err(reason) => Action::Copy {
                        reason: reason.into(),
                    },
                },
            );
        }
        Ok(Action::Copy {
            reason: "other game file".into(),
        })
    }
    fn plan_notes(
        &self,
        root: &Path,
        engine: &EngineInfo,
        settings: &ResolvedSettings,
        _: &[compressor_core::Entry],
    ) -> Vec<String> {
        data_folders(root)
            .unwrap_or_default()
            .iter()
            .map(
                |folder| match decision(root, folder, settings, engine.version_check_overridden) {
                    Ok(_) => format!("{}: player data will be archived", folder.display()),
                    Err(reason) => format!("{}: {reason}", folder.display()),
                },
            )
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerHeader {
    pub version: String,
    pub platform: i32,
}

pub fn read_player_header(path: &Path) -> io::Result<PlayerHeader> {
    let parse = || -> io::Result<PlayerHeader> {
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        let _metadata_size = be_u32(&mut file)?;
        let _file_size = be_u32(&mut file)?;
        let format = be_u32(&mut file)?;
        let _data_offset = be_u32(&mut file)?;
        if format < 9 {
            return Err(invalid("unsupported serialized file version"));
        }
        let mut endian = [0; 4];
        file.read_exact(&mut endian)?;
        if endian[0] > 1 || endian[1..] != [0, 0, 0] {
            return Err(invalid("invalid serialized file header"));
        }
        if format >= 22 {
            let _ = be_u32(&mut file)?;
            let _ = be_u64(&mut file)?;
            let _ = be_u64(&mut file)?;
            let _ = be_u64(&mut file)?;
        }
        if file.stream_position()? >= length {
            return Err(invalid("truncated serialized metadata"));
        }
        let version = cstring(&mut file)?;
        if !version.chars().next().is_some_and(|c| c.is_ascii_digit()) || !version.contains('.') {
            return Err(invalid("invalid Unity version"));
        }
        let mut bytes = [0; 4];
        file.read_exact(&mut bytes)?;
        let platform = if endian[0] == 0 {
            i32::from_le_bytes(bytes)
        } else {
            i32::from_be_bytes(bytes)
        };
        Ok(PlayerHeader { version, platform })
    };
    parse().map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            invalid("truncated serialized file")
        } else {
            e
        }
    })
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn align16(n: u64) -> u64 {
    (n + 15) & !15
}
fn write_padding(out: &mut impl Write, n: u64) -> io::Result<()> {
    out.write_all(&[0; 16][..n as usize])
}
fn compress(data: &[u8], level: i32) -> io::Result<Vec<u8>> {
    lz4::block::compress(
        data,
        Some(lz4::block::CompressionMode::HIGHCOMPRESSION(level)),
        false,
    )
    .map_err(io::Error::other)
}
struct Block {
    raw: u32,
    compressed: u32,
    flags: u16,
}
struct Directory {
    blocks: Vec<Block>,
    nodes: Vec<PackMember>,
    data_start: u64,
}
fn be_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}
fn be_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}
fn cstring(r: &mut impl Read) -> io::Result<String> {
    let mut bytes = Vec::new();
    loop {
        let mut b = [0];
        r.read_exact(&mut b)?;
        if b[0] == 0 {
            break;
        }
        if bytes.len() == 4096 {
            return Err(invalid("UnityFS string too long"));
        }
        bytes.push(b[0]);
    }
    String::from_utf8(bytes).map_err(|_| invalid("invalid UnityFS string"))
}
fn decode(data: &[u8], size: usize, flags: u16) -> io::Result<Vec<u8>> {
    let decoded = match flags & 0x3f {
        0 => data.to_vec(),
        2 | 3 => lz4::block::decompress(
            data,
            Some(i32::try_from(size).map_err(|_| invalid("block too large"))?),
        )
        .map_err(|_| invalid("invalid LZ4 block"))?,
        1 => {
            if data.len() < 5 {
                return Err(invalid("truncated LZMA properties"));
            }
            let mut header = data[..5].to_vec();
            header.extend_from_slice(&(size as u64).to_le_bytes());
            header.extend_from_slice(&data[5..]);
            let mut result = Vec::new();
            lzma_rs::lzma_decompress(&mut std::io::Cursor::new(header), &mut result)
                .map_err(|_| invalid("invalid LZMA block"))?;
            result
        }
        _ => return Err(invalid("unsupported UnityFS compression")),
    };
    if decoded.len() != size {
        return Err(invalid("UnityFS block size mismatch"));
    }
    Ok(decoded)
}
fn read_directory_inner(path: &Path) -> io::Result<Directory> {
    let read = || -> io::Result<Directory> {
        let mut file = File::open(path)?;
        let file_size = file.metadata()?.len();
        let mut magic = [0; 8];
        file.read_exact(&mut magic)?;
        if &magic != b"UnityFS\0" {
            return Err(invalid("not UnityFS"));
        }
        let version = be_u32(&mut file)?;
        let _unity = cstring(&mut file)?;
        let _revision = cstring(&mut file)?;
        let total = be_u64(&mut file)?;
        let compressed = be_u32(&mut file)? as usize;
        let raw = be_u32(&mut file)? as usize;
        let flags = be_u32(&mut file)?;
        if total != file_size || compressed as u64 > total || raw > 256 * 1024 * 1024 {
            return Err(invalid("invalid UnityFS sizes"));
        }
        let mut header_end = file.stream_position()?;
        if version >= 7 {
            header_end = align16(header_end);
        }
        let info_start = if flags & 0x80 != 0 {
            total - compressed as u64
        } else {
            header_end
        };
        let mut data_start = if flags & 0x80 != 0 {
            header_end
        } else {
            info_start + compressed as u64
        };
        if flags & 0x200 != 0 {
            data_start = align16(data_start);
        }
        file.seek(SeekFrom::Start(info_start))?;
        let mut info = vec![0; compressed];
        file.read_exact(&mut info)?;
        let info = decode(&info, raw, flags as u16)?;
        let mut cursor = std::io::Cursor::new(info);
        let mut hash = [0; 16];
        cursor.read_exact(&mut hash)?;
        let count = be_u32(&mut cursor)? as usize;
        if count > raw / 10 {
            return Err(invalid("invalid block count"));
        }
        let mut blocks = Vec::with_capacity(count);
        for _ in 0..count {
            let raw = be_u32(&mut cursor)?;
            let compressed = be_u32(&mut cursor)?;
            let mut flags = [0; 2];
            cursor.read_exact(&mut flags)?;
            blocks.push(Block {
                raw,
                compressed,
                flags: u16::from_be_bytes(flags),
            });
        }
        let count = be_u32(&mut cursor)? as usize;
        if count > raw / 21 {
            return Err(invalid("invalid node count"));
        }
        let mut nodes = Vec::with_capacity(count);
        let stream_size: u64 = blocks.iter().map(|b| u64::from(b.raw)).sum();
        let compressed_size: u64 = blocks.iter().map(|b| u64::from(b.compressed)).sum();
        if data_start
            .checked_add(compressed_size)
            .is_none_or(|end| end > if flags & 0x80 != 0 { info_start } else { total })
        {
            return Err(invalid("truncated UnityFS blocks"));
        }
        for _ in 0..count {
            let offset = be_u64(&mut cursor)?;
            let size = be_u64(&mut cursor)?;
            let _flags = be_u32(&mut cursor)?;
            let name = cstring(&mut cursor)?;
            if offset.checked_add(size).is_none_or(|end| end > stream_size) {
                return Err(invalid("invalid node bounds"));
            }
            nodes.push(PackMember { name, offset, size });
        }
        Ok(Directory {
            blocks,
            nodes,
            data_start,
        })
    };
    read().map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            invalid("truncated UnityFS archive")
        } else {
            e
        }
    })
}
fn flush_batch(
    batch: &mut Vec<Vec<u8>>,
    blocks: &mut Vec<Block>,
    output: &mut File,
    level: i32,
    cancel: &CancellationToken,
) -> io::Result<()> {
    if cancel.is_cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    let encoded: Vec<_> = batch
        .par_iter()
        .map(|data| compress(data, level))
        .collect::<io::Result<_>>()?;
    for (raw, compressed) in batch.drain(..).zip(encoded) {
        let use_compressed = compressed.len() < raw.len();
        let bytes = if use_compressed { &compressed } else { &raw };
        output.write_all(bytes)?;
        blocks.push(Block {
            raw: raw.len() as u32,
            compressed: bytes.len() as u32,
            flags: if use_compressed { 3 } else { 0 },
        });
    }
    Ok(())
}
impl UnityFs {
    fn write_inner(
        &self,
        members: &[ArchiveInput],
        output: &Path,
        params: &serde_json::Value,
        cancel: &CancellationToken,
    ) -> io::Result<()> {
        let version = params["format_version"]
            .as_u64()
            .filter(|v| *v == 7 || *v == 8)
            .ok_or_else(|| invalid("invalid UnityFS format version"))? as u32;
        let revision = params["unity_revision"]
            .as_str()
            .filter(|s| !s.contains('\0'))
            .ok_or_else(|| invalid("invalid Unity revision"))?;
        let padding = params["block_padding"]
            .as_bool()
            .ok_or_else(|| invalid("missing block padding"))?;
        let block_size = params["block_size"]
            .as_u64()
            .filter(|n| *n == 131072)
            .ok_or_else(|| invalid("invalid block size"))? as usize;
        let level = params["lz4hc_level"]
            .as_i64()
            .filter(|n| *n == 9)
            .ok_or_else(|| invalid("invalid compression level"))? as i32;
        let mut blocks_file = tempfile::tempfile_in(output.parent().unwrap_or(Path::new(".")))?;
        let mut blocks = Vec::new();
        let mut batch = Vec::new();
        let mut pending = Vec::with_capacity(block_size);
        let mut offset = 0u64;
        let mut nodes = Vec::new();
        for member in members {
            if member.name.contains('\0') {
                return Err(invalid("invalid archive member name"));
            }
            nodes.push((
                offset,
                member.size,
                if member.name.ends_with(".resS") {
                    0u32
                } else {
                    4u32
                },
                &member.name,
            ));
            offset = offset
                .checked_add(member.size)
                .ok_or_else(|| invalid("archive size overflow"))?;
            let mut file = File::open(&member.path)?;
            let mut hasher = blake3::Hasher::new();
            let mut size = 0u64;
            let mut buf = [0u8; 65536];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                size += n as u64;
                let mut remaining = &buf[..n];
                while !remaining.is_empty() {
                    let count = (block_size - pending.len()).min(remaining.len());
                    pending.extend_from_slice(&remaining[..count]);
                    remaining = &remaining[count..];
                    if pending.len() == block_size {
                        batch.push(std::mem::replace(
                            &mut pending,
                            Vec::with_capacity(block_size),
                        ));
                        if batch.len() == 256 {
                            flush_batch(&mut batch, &mut blocks, &mut blocks_file, level, cancel)?;
                        }
                    }
                }
            }
            if size != member.size || hasher.finalize().to_hex().as_str() != member.hash {
                return Err(invalid(&format!(
                    "stale archive member: {}",
                    member.path.display()
                )));
            }
        }
        if !pending.is_empty() {
            batch.push(pending);
        }
        flush_batch(&mut batch, &mut blocks, &mut blocks_file, level, cancel)?;
        let mut info = vec![0; 16];
        info.extend_from_slice(&(blocks.len() as u32).to_be_bytes());
        for block in &blocks {
            info.extend_from_slice(&block.raw.to_be_bytes());
            info.extend_from_slice(&block.compressed.to_be_bytes());
            info.extend_from_slice(&block.flags.to_be_bytes());
        }
        info.extend_from_slice(&(nodes.len() as u32).to_be_bytes());
        for (offset, size, flags, name) in nodes {
            info.extend_from_slice(&offset.to_be_bytes());
            info.extend_from_slice(&size.to_be_bytes());
            info.extend_from_slice(&flags.to_be_bytes());
            info.extend_from_slice(name.as_bytes());
            info.push(0);
        }
        let compressed = compress(&info, level)?;
        let mut header = Vec::new();
        header.extend_from_slice(b"UnityFS\0");
        header.extend_from_slice(&version.to_be_bytes());
        header.extend_from_slice(b"5.x.x\0");
        header.extend_from_slice(revision.as_bytes());
        header.push(0);
        let head_end = header.len() as u64 + 20;
        let info_start = align16(head_end);
        let info_end = info_start + compressed.len() as u64;
        let data_start = if padding { align16(info_end) } else { info_end };
        let total = data_start + blocks_file.seek(SeekFrom::End(0))?;
        let mut out = File::create(output)?;
        out.write_all(&header)?;
        out.write_all(&total.to_be_bytes())?;
        out.write_all(&(compressed.len() as u32).to_be_bytes())?;
        out.write_all(&(info.len() as u32).to_be_bytes())?;
        out.write_all(&(0x43u32 | if padding { 0x200 } else { 0 }).to_be_bytes())?;
        write_padding(&mut out, info_start - head_end)?;
        out.write_all(&compressed)?;
        write_padding(&mut out, data_start - info_end)?;
        blocks_file.seek(SeekFrom::Start(0))?;
        io::copy(&mut blocks_file, &mut out)?;
        if cancel.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        Ok(())
    }
}
impl ArchiveFormat for UnityFs {
    fn write(
        &self,
        members: &[ArchiveInput],
        output: &Path,
        params: &serde_json::Value,
        cancel: &CancellationToken,
    ) -> io::Result<()> {
        let result = self.write_inner(members, output, params, cancel);
        if result.is_err() {
            let _ = std::fs::remove_file(output);
        }
        result
    }
    fn read_directory(&self, archive: &Path) -> io::Result<Vec<PackMember>> {
        Ok(read_directory_inner(archive)?.nodes)
    }
    fn hash_member(&self, archive: &Path, member: &PackMember) -> io::Result<(u64, String)> {
        let directory = read_directory_inner(archive)?;
        if !directory.nodes.iter().any(|node| {
            node.name == member.name && node.offset == member.offset && node.size == member.size
        }) {
            return Err(invalid("archive member missing"));
        }
        let mut file = File::open(archive)?;
        let mut position = 0u64;
        // Compressed offset of the next block; seek only when a block is read.
        let mut source = directory.data_start;
        let mut hasher = blake3::Hasher::new();
        let mut count = 0u64;
        for block in directory.blocks {
            let end = position + u64::from(block.raw);
            if end > member.offset && position < member.offset + member.size {
                if file.stream_position()? != source {
                    file.seek(SeekFrom::Start(source))?;
                }
                let mut compressed = vec![0; block.compressed as usize];
                file.read_exact(&mut compressed)?;
                let data = decode(&compressed, block.raw as usize, block.flags)?;
                let start = member.offset.saturating_sub(position) as usize;
                let finish = (member.offset + member.size)
                    .min(end)
                    .saturating_sub(position) as usize;
                hasher.update(&data[start..finish]);
                count += (finish - start) as u64;
            }
            source += u64::from(block.compressed);
            position = end;
            if position >= member.offset + member.size {
                break;
            }
        }
        if count != member.size {
            return Err(invalid("truncated archive member"));
        }
        Ok((count, hasher.finalize().to_hex().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gathers_player_data_and_preserves_unsupported_builds() -> io::Result<()> {
        use compressor_core::{planning::build_plan, registry::TransformerRegistry, Resolved};
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let folder = root.join("Game_Data");
        std::fs::create_dir(&folder)?;
        std::fs::write(root.join("Game.exe"), b"player")?;
        let mut header = vec![0; 8];
        header.extend_from_slice(&22u32.to_be_bytes());
        header.extend_from_slice(&[0; 36]);
        header.extend_from_slice(b"2021.3.45f2\0");
        header.extend_from_slice(&19i32.to_le_bytes());
        std::fs::write(folder.join("globalgamemanagers"), &header)?;
        for name in [
            "level10",
            "level2",
            "resources.assets",
            "resources.assets.resS",
            "stream.resource",
        ] {
            std::fs::write(folder.join(name), b"data")?;
        }
        std::os::unix::fs::symlink("resources.assets", folder.join("linked.assets"))?;
        std::fs::create_dir(folder.join("StreamingAssets"))?;
        std::fs::write(folder.join("StreamingAssets/level0"), b"nested")?;
        let adapter = UnityAdapter {
            allow_unknown_version: false,
        };
        let engine = adapter.detect(root)?.unwrap();
        let settings = ResolvedSettings::default();
        let plan = build_plan(
            root,
            &adapter,
            engine.clone(),
            settings.clone(),
            &TransformerRegistry::default(),
        )?;
        let archives = adapter.archives(root, &engine, &settings)?;
        assert_eq!(archives.len(), 1);
        assert_eq!(
            archives[0]
                .members
                .iter()
                .map(|p| p.file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "globalgamemanagers",
                "level2",
                "level10",
                "resources.assets",
                "resources.assets.resS"
            ]
        );
        assert!(
            matches!(&plan.entries.iter().find(|e| e.path.0 == Path::new("Game_Data/stream.resource")).unwrap().action, Action::Copy { reason } if reason == "other game file")
        );
        assert!(matches!(
            &plan
                .entries
                .iter()
                .find(|e| e.path.0 == Path::new("Game_Data/StreamingAssets/level0"))
                .unwrap()
                .action,
            Action::Copy { .. }
        ));
        assert!(matches!(
            &plan
                .entries
                .iter()
                .find(|e| e.path.0 == Path::new("Game_Data/linked.assets"))
                .unwrap()
                .action,
            Action::Copy { .. }
        ));
        let mut disabled = settings.clone();
        disabled.engine_options.insert(
            "archive_data".into(),
            Resolved {
                value: serde_json::json!(false),
                source: "test".into(),
            },
        );
        assert!(adapter.archives(root, &engine, &disabled)?.is_empty());
        assert!(
            matches!(adapter.classify(root, Path::new("Game_Data/level2"), &engine, &disabled)?, Action::Copy { reason } if reason == "archiving disabled")
        );
        std::fs::write(folder.join("data.unity3d"), b"exists")?;
        assert!(
            matches!(adapter.classify(root, Path::new("Game_Data/level2"), &engine, &settings)?, Action::Copy { reason } if reason == "already compressed build")
        );
        std::fs::remove_file(folder.join("data.unity3d"))?;
        std::fs::write(folder.join("globalgamemanagers"), b"bad")?;
        assert!(
            matches!(adapter.classify(root, Path::new("Game_Data/level2"), &engine, &settings)?, Action::Copy { reason } if reason == "unreadable player data")
        );
        let mut old = header.clone();
        let pos = old.windows(12).position(|w| w == b"2021.3.45f2\0").unwrap();
        old[pos..pos + 12].copy_from_slice(b"2019.4.40f1\0");
        std::fs::write(folder.join("globalgamemanagers"), old)?;
        assert!(
            matches!(adapter.classify(root, Path::new("Game_Data/level2"), &engine, &settings)?, Action::Copy { reason } if reason == "unsupported Unity version")
        );
        Ok(())
    }
    #[test]
    fn version_table_boundaries() {
        for (version, expected) in [
            ("2020.3.33f1", 7),
            ("2020.3.34f1", 8),
            ("2021.3.1f1", 7),
            ("2021.3.2f1", 8),
            ("2022.1.0f1", 7),
            ("2022.1.1f1", 8),
        ] {
            assert_eq!(
                archive_params(version, false).unwrap()["format_version"],
                expected
            );
        }
        for version in ["2019.4.40f1", "unknown", "7000.0.0f1"] {
            assert!(archive_params(version, false).is_none());
        }
        assert!(archive_params("2019.4.40f1", true).is_none());
        assert_eq!(archive_params("7000.0", true).unwrap()["format_version"], 8);
    }
    #[test]
    fn detects_players_at_root_and_one_level_down() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        for folder in [
            "A_Data",
            "nested/B_Data",
            "missing_Data",
            "deep/child/C_Data",
        ] {
            std::fs::create_dir_all(root.join(folder))?;
            std::fs::write(root.join(folder).join("globalgamemanagers"), b"bad")?;
        }
        for file in ["A.exe", "nested/B.x86_64", "deep/child/C.x86"] {
            std::fs::write(root.join(file), b"")?;
        }
        assert_eq!(
            data_folders(root)?,
            vec![PathBuf::from("A_Data"), PathBuf::from("nested/B_Data")]
        );
        let adapter = UnityAdapter {
            allow_unknown_version: false,
        };
        let info = adapter.detect(root)?.unwrap();
        assert_eq!(info.name, "Unity");
        assert!(info.version.is_none());
        std::fs::remove_file(root.join("A.exe"))?;
        std::fs::remove_file(root.join("nested/B.x86_64"))?;
        assert!(adapter.detect(root)?.is_none());
        Ok(())
    }
    #[test]
    fn reads_serialized_headers_and_rejects_garbage() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("header");
        for format in [17u32, 22] {
            for endian in [0u8, 1] {
                let mut bytes = vec![0; 8];
                bytes.extend_from_slice(&format.to_be_bytes());
                bytes.extend_from_slice(&[0; 4]);
                bytes.extend_from_slice(&[endian, 0, 0, 0]);
                if format == 22 {
                    bytes.extend_from_slice(&[0; 28]);
                }
                bytes.extend_from_slice(b"2022.3.7f1\0");
                bytes.extend_from_slice(&if endian == 0 {
                    19i32.to_le_bytes()
                } else {
                    19i32.to_be_bytes()
                });
                std::fs::write(&path, bytes)?;
                assert_eq!(
                    read_player_header(&path)?,
                    PlayerHeader {
                        version: "2022.3.7f1".into(),
                        platform: 19
                    }
                );
            }
        }
        std::fs::write(&path, b"garbage")?;
        assert_eq!(
            read_player_header(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        Ok(())
    }
    #[test]
    #[ignore = "requires local Unity test games"]
    fn reads_real_player_headers() -> io::Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../example_games");
        for (path, version) in [
            (
                "UNITY_1_Third_Crisis_1.1.0_Windows/Third Crisis_Data/globalgamemanagers",
                "2022.3.7f1",
            ),
            (
                "UNITY_2_Manor Windows/Manor_Data/globalgamemanagers",
                "2021.3.45f2",
            ),
            (
                "UNITY_3_NTRaholic/NTRaholic/Ntraholic_Data/globalgamemanagers",
                "2020.3.25f1",
            ),
        ] {
            assert_eq!(read_player_header(&root.join(path))?.version, version);
        }
        Ok(())
    }
    #[test]
    fn cancellation_removes_partial_archive() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("large");
        let data = vec![42; 64 * 1024 * 1024];
        std::fs::write(&source, &data)?;
        let input = ArchiveInput {
            name: "large".into(),
            path: source,
            size: data.len() as u64,
            hash: blake3::hash(&data).to_hex().to_string(),
        };
        let output = temp.path().join("archive");
        let token = CancellationToken::new();
        let signal = token.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            signal.cancel();
        });
        let params = serde_json::json!({"format_version":8,"unity_revision":"2021.3.45f2","block_padding":true,"block_size":131072,"lz4hc_level":9});
        let result = UnityFs.write(&[input], &output, &params, &token);
        thread.join().unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(!output.exists());
        Ok(())
    }
    #[test]
    fn writer_headers_and_round_trip() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let mut random = Vec::new();
        let mut n = 7u32;
        for _ in 0..150_000 {
            n ^= n << 13;
            n ^= n >> 17;
            n ^= n << 5;
            random.push(n as u8);
        }
        let files = [
            ("level0", vec![42; 280_000]),
            ("empty.assets", vec![]),
            ("random.resS", random),
        ];
        let mut inputs = Vec::new();
        for (name, bytes) in &files {
            let path = temp.path().join(name);
            std::fs::write(&path, bytes)?;
            inputs.push(ArchiveInput {
                name: (*name).into(),
                path,
                size: bytes.len() as u64,
                hash: blake3::hash(bytes).to_hex().to_string(),
            });
        }
        for version in [7, 8] {
            let out = temp.path().join(format!("{version}.unity3d"));
            let params = serde_json::json!({"format_version":version,"unity_revision":"2021.3.45f2","block_padding":version == 8,"block_size":131072,"lz4hc_level":9});
            UnityFs.write(&inputs, &out, &params, &CancellationToken::new())?;
            let data = std::fs::read(&out)?;
            assert_eq!(&data[..8], b"UnityFS\0");
            assert_eq!(&data[8..12], &(version as u32).to_be_bytes());
            let mut cursor = std::io::Cursor::new(&data[12..]);
            assert_eq!(cstring(&mut cursor)?, "5.x.x");
            assert_eq!(cstring(&mut cursor)?, "2021.3.45f2");
            assert_eq!(be_u64(&mut cursor)?, data.len() as u64);
            let compressed = be_u32(&mut cursor)?;
            let raw = be_u32(&mut cursor)?;
            let flags = be_u32(&mut cursor)?;
            assert!(compressed > 0 && raw > 0);
            assert_eq!(flags, if version == 8 { 0x243 } else { 0x43 });
            let info_start = align16(12 + cursor.position()) as usize;
            let info = decode(
                &data[info_start..info_start + compressed as usize],
                raw as usize,
                flags as u16,
            )?;
            let mut parsed = std::io::Cursor::new(info);
            parsed.set_position(16);
            let count = be_u32(&mut parsed)?;
            let mut raw_blocks = 0;
            for _ in 0..count {
                let _ = be_u32(&mut parsed)?;
                let _ = be_u32(&mut parsed)?;
                let mut bytes = [0; 2];
                parsed.read_exact(&mut bytes)?;
                if u16::from_be_bytes(bytes) == 0 {
                    raw_blocks += 1;
                }
            }
            assert!(raw_blocks > 0);
            assert_eq!(be_u32(&mut parsed)?, 3);
            for (index, (name, _)) in files.iter().enumerate() {
                let _ = be_u64(&mut parsed)?;
                let _ = be_u64(&mut parsed)?;
                assert_eq!(be_u32(&mut parsed)?, if index == 2 { 0 } else { 4 });
                assert_eq!(cstring(&mut parsed)?, *name);
            }
            let directory = UnityFs.read_directory(&out)?;
            assert_eq!(directory.len(), files.len());
            for (node, input) in directory.iter().zip(&inputs) {
                assert_eq!(node.name, input.name);
                assert_eq!(
                    UnityFs.hash_member(&out, node)?,
                    (input.size, input.hash.clone())
                );
            }
            let truncated = temp.path().join("truncated");
            std::fs::write(&truncated, &data[..data.len() - 1])?;
            assert_eq!(
                UnityFs.read_directory(&truncated).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        std::fs::write(&inputs[0].path, b"changed")?;
        let output = temp.path().join("stale");
        let params = serde_json::json!({"format_version":8,"unity_revision":"2021.3.45f2","block_padding":true,"block_size":131072,"lz4hc_level":9});
        assert!(UnityFs
            .write(&inputs, &output, &params, &CancellationToken::new())
            .unwrap_err()
            .to_string()
            .contains("stale archive member"));
        assert!(!output.exists());
        Ok(())
    }
    #[test]
    #[ignore = "requires the Manor test game"]
    fn reads_real_manor_bundle() -> io::Result<()> {
        let root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../example_games/UNITY_2_Manor Windows");
        let path = root.join("Manor_Data/StreamingAssets/sprites/door");
        assert_eq!(UnityFs.read_directory(&path)?.len(), 2);
        Ok(())
    }
}
