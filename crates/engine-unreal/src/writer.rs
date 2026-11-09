//! Rewrites a raw IoStore container with Zlib blocks, and restores the original bytes.

use crate::toc::{
    Toc, BLOCK_ENTRY_SIZE, FLAG_COMPRESSED, FLAG_ENCRYPTED, FLAG_SIGNED, META_COMPRESSED,
    META_ENTRY_SIZE, METHOD_NAME_LENGTH,
};
use compressor_core::registry::{RewriteFormat, RewriteInput};
use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};
use rayon::prelude::*;
use std::{
    fs::File,
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
};
use tokio_util::sync::CancellationToken;

pub const METHOD_NAME: &str = "Zlib";
/// Blocks are read, compressed and written in batches of about this many bytes.
const BATCH_BYTES: u64 = 64 << 20;
const ALIGNMENT: u64 = 16;
/// Shader code library, shader code, and the container header stay raw.
const RAW_CHUNK_TYPES: [u8; 3] = [6, 8, 9];

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "cancelled")
}

fn check(cancel: &CancellationToken) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(cancelled())
    } else {
        Ok(())
    }
}

fn parse(bytes: &[u8]) -> io::Result<Toc> {
    Toc::parse(bytes).map_err(|e| invalid(e.to_string()))
}

/// Compress with Zlib level 9; `None` when the result is not smaller.
fn deflate(data: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let mut encoder = ZlibEncoder::new(Vec::with_capacity(data.len() / 2), Compression::new(9));
    encoder.write_all(data)?;
    let out = encoder.finish()?;
    Ok((out.len() < data.len()).then_some(out))
}

fn inflate(data: &[u8], expected: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(expected);
    ZlibDecoder::new(data).read_to_end(&mut out)?;
    if out.len() != expected {
        return Err(invalid("block decodes to the wrong size"));
    }
    Ok(out)
}

fn align(position: u64) -> u64 {
    position.div_ceil(ALIGNMENT) * ALIGNMENT
}

/// Per block: whether it must stay raw because its chunk is of an always-raw kind.
fn raw_blocks(toc: &Toc) -> Vec<bool> {
    let mut raw = vec![false; toc.blocks.len()];
    for chunk in 0..toc.chunk_ids.len() {
        if RAW_CHUNK_TYPES.contains(&toc.chunk_type(chunk)) {
            for block in toc.chunk_blocks(chunk) {
                raw[block] = true;
            }
        }
    }
    raw
}

fn require_raw(toc: &Toc, ucas_len: u64) -> io::Result<()> {
    if toc.flags & (FLAG_COMPRESSED | FLAG_ENCRYPTED | FLAG_SIGNED) != 0
        || toc.partition_count != 1
        || !toc.method_names.is_empty()
        || !toc.is_raw_and_contiguous(ucas_len)
    {
        return Err(invalid("container is not a raw single-partition container"));
    }
    Ok(())
}

/// Batches of consecutive block indexes holding about [`BATCH_BYTES`] each.
fn batches(toc: &Toc, limit: u64) -> Vec<std::ops::Range<usize>> {
    let mut result = Vec::new();
    let (mut start, mut bytes) = (0, 0u64);
    for (index, block) in toc.blocks.iter().enumerate() {
        bytes += u64::from(block.compressed);
        if bytes >= limit {
            result.push(start..index + 1);
            start = index + 1;
            bytes = 0;
        }
    }
    if start < toc.blocks.len() {
        result.push(start..toc.blocks.len());
    }
    result
}

fn read_range(
    file: &mut File,
    toc: &Toc,
    range: &std::ops::Range<usize>,
) -> io::Result<Vec<Vec<u8>>> {
    file.seek(SeekFrom::Start(toc.blocks[range.start].offset))?;
    range
        .clone()
        .map(|index| {
            let mut data = vec![0u8; toc.blocks[index].compressed as usize];
            file.read_exact(&mut data)?;
            Ok(data)
        })
        .collect()
}

/// The new `.utoc`: Zlib method name, compressed flags, and the new block table.
fn build_toc(old: &[u8], toc: &Toc, new_blocks: &[(u64, u32, u32, u8)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(old.len() + METHOD_NAME_LENGTH);
    out.extend(&old[..toc.blocks_offset]);
    out[80] |= FLAG_COMPRESSED;
    out[36..40].copy_from_slice(&1u32.to_le_bytes());
    out[40..44].copy_from_slice(&(METHOD_NAME_LENGTH as u32).to_le_bytes());
    for (offset, compressed, uncompressed, method) in new_blocks {
        let value = u128::from(*offset)
            | u128::from(*compressed) << 40
            | u128::from(*uncompressed) << 64
            | u128::from(*method) << 88;
        out.extend(&value.to_le_bytes()[..BLOCK_ENTRY_SIZE]);
    }
    let mut name = METHOD_NAME.as_bytes().to_vec();
    name.resize(METHOD_NAME_LENGTH, 0);
    out.extend(name);
    out.extend(&old[toc.methods_offset..]);
    // The meta section moved by the method name; set each chunk's compressed flag.
    let metas = toc.metas_offset + METHOD_NAME_LENGTH;
    for chunk in 0..toc.chunk_ids.len() {
        let any = toc
            .chunk_blocks(chunk)
            .any(|block| new_blocks[block].3 != 0);
        let flag = &mut out[metas + chunk * META_ENTRY_SIZE + 20];
        if any {
            *flag |= META_COMPRESSED;
        } else {
            *flag &= !META_COMPRESSED;
        }
    }
    out
}

/// Write `ucas_out` and `utoc_out` for a raw container.
pub fn rewrite(
    utoc: &Path,
    ucas: &Path,
    utoc_out: &Path,
    ucas_out: &Path,
    cancel: &CancellationToken,
) -> io::Result<()> {
    rewrite_batched(utoc, ucas, utoc_out, ucas_out, cancel, BATCH_BYTES)
}

fn rewrite_batched(
    utoc: &Path,
    ucas: &Path,
    utoc_out: &Path,
    ucas_out: &Path,
    cancel: &CancellationToken,
    batch_bytes: u64,
) -> io::Result<()> {
    let old = std::fs::read(utoc)?;
    let toc = parse(&old)?;
    let mut input = File::open(ucas)?;
    require_raw(&toc, input.metadata()?.len())?;
    let raw = raw_blocks(&toc);
    let mut output = BufWriter::with_capacity(1 << 20, File::create(ucas_out)?);
    let mut new_blocks = Vec::with_capacity(toc.blocks.len());
    let mut position = 0u64;
    for range in batches(&toc, batch_bytes) {
        check(cancel)?;
        let data = read_range(&mut input, &toc, &range)?;
        let packed: Vec<Option<Vec<u8>>> = data
            .par_iter()
            .zip(&raw[range.clone()])
            .map(|(block, keep_raw)| if *keep_raw { Ok(None) } else { deflate(block) })
            .collect::<io::Result<_>>()?;
        for ((index, original), packed) in range.zip(data).zip(packed) {
            let (bytes, method) = match &packed {
                Some(packed) => (packed.as_slice(), 1u8),
                None => (original.as_slice(), 0u8),
            };
            let start = align(position);
            output.write_all(&vec![0u8; (start - position) as usize])?;
            output.write_all(bytes)?;
            new_blocks.push((
                start,
                bytes.len() as u32,
                toc.blocks[index].uncompressed,
                method,
            ));
            position = start + bytes.len() as u64;
        }
    }
    let end = align(position);
    output.write_all(&vec![0u8; (end - position) as usize])?;
    output.flush()?;
    output.get_ref().sync_all()?;
    check(cancel)?;
    std::fs::write(utoc_out, build_toc(&old, &toc, &new_blocks))?;
    Ok(())
}

fn hex(hasher: &blake3::Hasher) -> String {
    hasher.finalize().to_hex().to_string()
}

/// Recreate the input `.utoc` from a rewritten one and return its size and hash.
fn restore_toc(bytes: &[u8], toc: &Toc) -> io::Result<(u64, String)> {
    if toc.method_names != [METHOD_NAME] || toc.flags & FLAG_COMPRESSED == 0 {
        return Err(invalid("container does not use Zlib"));
    }
    let mut out = Vec::with_capacity(bytes.len());
    out.extend(&bytes[..toc.blocks_offset]);
    out[80] &= !FLAG_COMPRESSED;
    out[36..40].copy_from_slice(&0u32.to_le_bytes());
    let mut position = 0u64;
    for block in &toc.blocks {
        let value = u128::from(position)
            | u128::from(block.uncompressed) << 40
            | u128::from(block.uncompressed) << 64;
        out.extend(&value.to_le_bytes()[..BLOCK_ENTRY_SIZE]);
        position += u64::from(block.uncompressed);
    }
    let names_end = toc.methods_offset + toc.method_names.len() * toc.method_name_length as usize;
    out.extend(&bytes[names_end..]);
    let metas = toc.metas_offset - toc.method_names.len() * toc.method_name_length as usize;
    for chunk in 0..toc.chunk_ids.len() {
        out[metas + chunk * META_ENTRY_SIZE + 20] &= !META_COMPRESSED;
    }
    Ok((out.len() as u64, blake3::hash(&out).to_hex().to_string()))
}

/// Decode every block in order and hash the bytes, which are the input `.ucas`.
fn restore_ucas(ucas: &Path, toc: &Toc, cancel: &CancellationToken) -> io::Result<(u64, String)> {
    let mut file = File::open(ucas)?;
    let length = file.metadata()?.len();
    let mut hasher = blake3::Hasher::new();
    let mut total = 0u64;
    let mut start = 0;
    while start < toc.blocks.len() {
        check(cancel)?;
        let mut end = start;
        let mut bytes = 0u64;
        while end < toc.blocks.len() && bytes < BATCH_BYTES {
            bytes += u64::from(toc.blocks[end].compressed);
            end += 1;
        }
        let batch: Vec<(Vec<u8>, usize, u8)> = (start..end)
            .map(|index| {
                let block = &toc.blocks[index];
                if block.offset + u64::from(block.compressed) > length {
                    return Err(invalid("block lies outside the file"));
                }
                file.seek(SeekFrom::Start(block.offset))?;
                let mut data = vec![0u8; block.compressed as usize];
                file.read_exact(&mut data)?;
                Ok((data, block.uncompressed as usize, block.method))
            })
            .collect::<io::Result<_>>()?;
        let decoded: Vec<Vec<u8>> = batch
            .into_par_iter()
            .map(|(data, size, method)| match method {
                0 if data.len() == size => Ok(data),
                1 => inflate(&data, size),
                _ => Err(invalid("block has an unknown method or wrong size")),
            })
            .collect::<io::Result<_>>()?;
        for block in decoded {
            hasher.update(&block);
            total += block.len() as u64;
        }
        start = end;
    }
    Ok((total, hex(&hasher)))
}

/// Sample blocks by even steps and project the rewritten pair's size over the input's.
pub fn estimate_ratio(
    utoc: &Path,
    ucas: &Path,
    sample_blocks: usize,
    cancel: &CancellationToken,
) -> io::Result<Option<f64>> {
    let old = std::fs::read(utoc)?;
    let toc = parse(&old)?;
    let mut input = File::open(ucas)?;
    let ucas_len = input.metadata()?.len();
    require_raw(&toc, ucas_len)?;
    let raw = raw_blocks(&toc);
    let candidates: Vec<usize> = (0..toc.blocks.len()).filter(|i| !raw[*i]).collect();
    if candidates.is_empty() || sample_blocks == 0 {
        return Ok(None);
    }
    let count = sample_blocks.min(candidates.len());
    let mut sampled = Vec::with_capacity(count);
    for step in 0..count {
        check(cancel)?;
        let block = &toc.blocks[candidates[step * candidates.len() / count]];
        input.seek(SeekFrom::Start(block.offset))?;
        let mut data = vec![0u8; block.compressed as usize];
        input.read_exact(&mut data)?;
        sampled.push(data);
    }
    let (packed, original) = sampled
        .par_iter()
        .map(|data| {
            deflate(data).map(|p| (p.map_or(data.len(), |p| p.len()) as u64, data.len() as u64))
        })
        .try_reduce(|| (0, 0), |a, b| Ok((a.0 + b.0, a.1 + b.1)))?;
    check(cancel)?;
    let candidate_bytes: u64 = candidates
        .iter()
        .map(|i| u64::from(toc.blocks[*i].compressed))
        .sum();
    let ratio = packed as f64 / original as f64;
    let raw_bytes = ucas_len - candidate_bytes;
    let padding = toc.blocks.len() as u64 * (ALIGNMENT / 2);
    let estimated = raw_bytes as f64 + candidate_bytes as f64 * ratio + padding as f64;
    let output = estimated + old.len() as f64 + METHOD_NAME_LENGTH as f64;
    Ok(Some(output / (ucas_len + old.len() as u64) as f64))
}

/// The Unreal IoStore rewrite format.
pub struct UnrealIoStore;

impl RewriteFormat for UnrealIoStore {
    fn estimate_ratio(
        &self,
        primary: &RewriteInput,
        partner: &RewriteInput,
        sample_blocks: usize,
        cancel: &CancellationToken,
    ) -> io::Result<Option<f64>> {
        estimate_ratio(&partner.path, &primary.path, sample_blocks, cancel)
    }
    fn write(
        &self,
        primary: &RewriteInput,
        partner: &RewriteInput,
        primary_output: &Path,
        partner_output: &Path,
        _: &serde_json::Value,
        cancel: &CancellationToken,
    ) -> io::Result<()> {
        rewrite(
            &partner.path,
            &primary.path,
            partner_output,
            primary_output,
            cancel,
        )
    }
    fn restore_hashes(
        &self,
        primary_output: &Path,
        partner_output: &Path,
        cancel: &CancellationToken,
    ) -> io::Result<((u64, String), (u64, String))> {
        let bytes = std::fs::read(partner_output)?;
        let toc = parse(&bytes)?;
        let utoc = restore_toc(&bytes, &toc)?;
        let ucas = restore_ucas(primary_output, &toc, cancel)?;
        Ok((ucas, utoc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compressor_test_fixtures::iostore::{self, Chunk, Options};
    use std::path::PathBuf;

    fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    struct Files {
        dir: tempfile::TempDir,
        original: iostore::Built,
    }
    impl Files {
        fn new(chunks: &[Chunk]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let original = iostore::build(chunks, &Options::default());
            std::fs::write(dir.path().join("in.utoc"), &original.utoc).unwrap();
            std::fs::write(dir.path().join("in.ucas"), &original.ucas).unwrap();
            Self { dir, original }
        }
        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }
        fn rewrite(&self, batch: u64) -> io::Result<()> {
            rewrite_batched(
                &self.path("in.utoc"),
                &self.path("in.ucas"),
                &self.path("out.utoc"),
                &self.path("out.ucas"),
                &CancellationToken::new(),
                batch,
            )
        }
        fn restore(&self) -> io::Result<((u64, String), (u64, String))> {
            UnrealIoStore.restore_hashes(
                &self.path("out.ucas"),
                &self.path("out.utoc"),
                &CancellationToken::new(),
            )
        }
    }

    fn mixed() -> Vec<Chunk> {
        vec![
            Chunk {
                kind: 2,
                data: iostore::sample_bytes(1, 40_000),
            },
            Chunk {
                kind: 1,
                data: iostore::sample_bytes(5, 9_000),
            },
            Chunk {
                kind: 9,
                data: iostore::sample_bytes(2, 12_000),
            },
            Chunk {
                kind: 8,
                data: iostore::sample_bytes(3, 4_096),
            },
            Chunk {
                kind: 6,
                data: iostore::sample_bytes(4, 300),
            },
            Chunk {
                kind: 2,
                data: noise(20_000),
            },
            Chunk {
                kind: 3,
                data: Vec::new(),
            },
        ]
    }

    #[test]
    fn writes_zlib_blocks_by_the_layout_rules() {
        let files = Files::new(&mixed());
        files.rewrite(BATCH_BYTES).unwrap();
        let old = Toc::parse(&files.original.utoc).unwrap();
        let new_bytes = std::fs::read(files.path("out.utoc")).unwrap();
        let new = Toc::parse(&new_bytes).unwrap();
        let out = std::fs::read(files.path("out.ucas")).unwrap();
        assert_eq!(out.len() % 16, 0);
        assert_eq!(new.method_names, vec![METHOD_NAME]);
        assert_eq!(new.method_name_length, 32);
        assert_eq!(new.flags, old.flags | FLAG_COMPRESSED);
        // Chunk table, block size, and uncompressed layout are unchanged.
        assert_eq!(
            (&new.chunk_ids, &new.offset_lengths, new.block_size),
            (&old.chunk_ids, &old.offset_lengths, old.block_size)
        );
        assert_eq!(new.blocks.len(), old.blocks.len());
        let raw = raw_blocks(&old);
        let mut compressed = 0;
        for (index, (a, b)) in old.blocks.iter().zip(&new.blocks).enumerate() {
            assert_eq!(b.offset % 16, 0, "block {index} is aligned");
            assert_eq!(b.uncompressed, a.uncompressed);
            let original = &files.original.ucas[a.offset as usize..][..a.compressed as usize];
            let stored = &out[b.offset as usize..][..b.compressed as usize];
            if b.method == 1 {
                compressed += 1;
                assert!(!raw[index], "shader and header blocks stay raw");
                assert!(b.compressed < a.compressed);
                assert_eq!(inflate(stored, a.uncompressed as usize).unwrap(), original);
            } else {
                assert_eq!(stored, original);
            }
        }
        assert!(compressed > 5);
        // Compressible shader blocks stayed raw even though Zlib would shrink them.
        assert!(deflate(
            &files.original.ucas
                [old.blocks[raw.iter().position(|r| *r).unwrap()].offset as usize..][..4096]
        )
        .unwrap()
        .is_some());
        // Noise chunk blocks are not smaller, so they stay raw.
        let noise_blocks = old.chunk_blocks(5);
        assert!(noise_blocks.clone().all(|i| new.blocks[i].method == 0));
        // A chunk is flagged compressed when any of its blocks is.
        for chunk in 0..new.chunk_ids.len() {
            let any = new.chunk_blocks(chunk).any(|i| new.blocks[i].method != 0);
            assert_eq!(
                new.meta_flags[chunk] & META_COMPRESSED != 0,
                any,
                "chunk {chunk}"
            );
        }
        // Everything outside the block table, names, and meta flag bytes is untouched.
        assert_eq!(&new_bytes[..36], &files.original.utoc[..36]);
        assert_eq!(&new_bytes[44..80], &files.original.utoc[44..80]);
        let tail = old.methods_offset;
        assert_eq!(
            &new_bytes
                [new.methods_offset + 32..new.methods_offset + 32 + (old.metas_offset - tail)],
            &files.original.utoc[tail..old.metas_offset]
        );
    }

    #[test]
    fn small_batches_give_the_same_files() {
        let files = Files::new(&mixed());
        files.rewrite(BATCH_BYTES).unwrap();
        let first = (
            std::fs::read(files.path("out.utoc")).unwrap(),
            std::fs::read(files.path("out.ucas")).unwrap(),
        );
        files.rewrite(5000).unwrap();
        assert_eq!(first.0, std::fs::read(files.path("out.utoc")).unwrap());
        assert_eq!(first.1, std::fs::read(files.path("out.ucas")).unwrap());
    }

    #[test]
    fn restore_recreates_the_input_hashes() {
        let files = Files::new(&mixed());
        files.rewrite(5000).unwrap();
        let ((ucas_size, ucas_hash), (utoc_size, utoc_hash)) = files.restore().unwrap();
        assert_eq!(ucas_size, files.original.ucas.len() as u64);
        assert_eq!(
            ucas_hash,
            blake3::hash(&files.original.ucas).to_hex().as_str()
        );
        assert_eq!(utoc_size, files.original.utoc.len() as u64);
        assert_eq!(
            utoc_hash,
            blake3::hash(&files.original.utoc).to_hex().as_str()
        );
    }

    #[test]
    fn one_changed_byte_in_either_file_is_detected() {
        let files = Files::new(&mixed());
        files.rewrite(BATCH_BYTES).unwrap();
        let expected = files.restore().unwrap();
        let toc = Toc::parse(&std::fs::read(files.path("out.utoc")).unwrap()).unwrap();
        let utoc_len = std::fs::metadata(files.path("out.utoc")).unwrap().len() as usize;
        for name in ["out.ucas", "out.utoc"] {
            let original = std::fs::read(files.path(name)).unwrap();
            let positions: Vec<usize> = if name == "out.ucas" {
                // A compressed block, a raw block, and the last byte of the first block.
                let first = toc.blocks[0];
                vec![first.offset as usize + 3, toc.blocks[3].offset as usize + 1]
            } else {
                vec![
                    toc.blocks_offset + 3,
                    toc.metas_offset + 5,
                    utoc_len - 2,
                    toc.methods_offset - 1,
                ]
            };
            for at in positions {
                let mut changed = original.clone();
                changed[at] ^= 0x40;
                std::fs::write(files.path(name), &changed).unwrap();
                match files.restore() {
                    Err(_) => {}
                    Ok(restored) => assert_ne!(restored, expected, "{name} byte {at}"),
                }
            }
            std::fs::write(files.path(name), &original).unwrap();
        }
        assert_eq!(files.restore().unwrap(), expected);
    }

    #[test]
    fn rejects_inputs_that_are_not_raw_and_stops_when_cancelled() {
        let files = Files::new(&mixed());
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let error = rewrite(
            &files.path("in.utoc"),
            &files.path("in.ucas"),
            &files.path("out.utoc"),
            &files.path("out.ucas"),
            &cancelled,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(
            !files.path("out.utoc").exists(),
            "no table of contents without the blocks"
        );
        files.rewrite(BATCH_BYTES).unwrap();
        // A rewritten container is not a valid input.
        let error = rewrite(
            &files.path("out.utoc"),
            &files.path("out.ucas"),
            &files.path("again.utoc"),
            &files.path("again.ucas"),
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn sampled_estimate_is_close_to_the_full_ratio() {
        let mut chunks = Vec::new();
        for i in 0..60u8 {
            chunks.push(Chunk {
                kind: 2,
                data: if i % 3 == 0 {
                    noise(8000)
                } else {
                    iostore::sample_bytes(i, 8000 + 100 * i as usize)
                },
            });
        }
        let files = Files::new(&chunks);
        files.rewrite(BATCH_BYTES).unwrap();
        let input = (files.original.ucas.len() + files.original.utoc.len()) as f64;
        let output = (std::fs::metadata(files.path("out.ucas")).unwrap().len()
            + std::fs::metadata(files.path("out.utoc")).unwrap().len()) as f64;
        let estimate = estimate_ratio(
            &files.path("in.utoc"),
            &files.path("in.ucas"),
            24,
            &CancellationToken::new(),
        )
        .unwrap()
        .unwrap();
        assert!(
            (estimate - output / input).abs() < 0.05,
            "{estimate} vs {}",
            output / input
        );
    }
}
