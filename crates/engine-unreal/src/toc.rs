//! Reader for IoStore `.utoc` files, table-of-contents version 8.

use std::{fmt, ops::Range};

pub const MAGIC: &[u8; 16] = b"-==--==--==--==-";
pub const HEADER_SIZE: usize = 144;
pub const SUPPORTED_VERSION: u8 = 8;
pub const BLOCK_ENTRY_SIZE: usize = 12;
pub const META_ENTRY_SIZE: usize = 24;
pub const METHOD_NAME_LENGTH: usize = 32;

pub const FLAG_COMPRESSED: u8 = 1;
pub const FLAG_ENCRYPTED: u8 = 2;
pub const FLAG_SIGNED: u8 = 4;
pub const FLAG_INDEXED: u8 = 8;
pub const FLAG_ON_DEMAND: u8 = 16;
pub const META_COMPRESSED: u8 = 1;
pub const META_MEMORY_MAPPED: u8 = 2;

/// Why a `.utoc` could not be read.
#[derive(Debug, PartialEq, Eq)]
pub enum TocError {
    /// Too short, wrong magic, or sections that do not fit in the file.
    Unreadable(String),
    /// A valid IoStore file with another table-of-contents version.
    UnsupportedVersion(u8),
}

impl fmt::Display for TocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(why) => write!(f, "unreadable container: {why}"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported container version {v}"),
        }
    }
}
impl std::error::Error for TocError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    /// Offset in the `.ucas`.
    pub offset: u64,
    pub compressed: u32,
    pub uncompressed: u32,
    /// 0 is raw; n selects `method_names[n - 1]`.
    pub method: u8,
}

#[derive(Clone, Debug)]
pub struct Toc {
    pub flags: u8,
    pub block_size: u32,
    pub partition_count: u32,
    pub directory_index_size: u32,
    pub perfect_hash_seeds: u32,
    pub chunks_without_perfect_hash: u32,
    pub block_entry_size: u32,
    pub method_name_length: u32,
    pub method_names: Vec<String>,
    /// Per chunk: the 12-byte chunk id; its last byte is the chunk type.
    pub chunk_ids: Vec<[u8; 12]>,
    /// Per chunk: (offset, length) in uncompressed space.
    pub offset_lengths: Vec<(u64, u64)>,
    pub blocks: Vec<Block>,
    /// Per chunk: the meta flags byte.
    pub meta_flags: Vec<u8>,
    /// Byte offsets inside the `.utoc` of the sections the writer edits.
    pub blocks_offset: usize,
    pub methods_offset: usize,
    pub metas_offset: usize,
}

fn unreadable(why: &str) -> TocError {
    TocError::Unreadable(why.to_owned())
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("bounds checked"))
}

/// Add `count * size` to `at`, failing when the section would pass `limit`.
fn section(at: usize, count: usize, size: usize, limit: usize) -> Result<usize, TocError> {
    count
        .checked_mul(size)
        .and_then(|n| n.checked_add(at))
        .filter(|end| *end <= limit)
        .ok_or_else(|| unreadable("sections extend past the end of the file"))
}

impl Toc {
    pub fn parse(bytes: &[u8]) -> Result<Self, TocError> {
        if bytes.len() < 20 {
            return Err(unreadable("file is too short"));
        }
        if &bytes[..16] != MAGIC {
            return Err(unreadable("bad magic"));
        }
        if bytes[16] != SUPPORTED_VERSION {
            return Err(TocError::UnsupportedVersion(bytes[16]));
        }
        if bytes.len() < HEADER_SIZE {
            return Err(unreadable("header is too short"));
        }
        if u32_at(bytes, 20) as usize != HEADER_SIZE {
            return Err(unreadable("unexpected header size"));
        }
        let entries = u32_at(bytes, 24) as usize;
        let block_count = u32_at(bytes, 28) as usize;
        let block_entry_size = u32_at(bytes, 32);
        let method_count = u32_at(bytes, 36) as usize;
        let method_name_length = u32_at(bytes, 40);
        let block_size = u32_at(bytes, 44);
        let directory_index_size = u32_at(bytes, 48);
        let partition_count = u32_at(bytes, 52);
        let flags = bytes[80];
        let seeds = u32_at(bytes, 84);
        let without_hash = u32_at(bytes, 96);
        if block_entry_size as usize != BLOCK_ENTRY_SIZE || block_size == 0 {
            return Err(unreadable("unexpected block entry or block size"));
        }
        let limit = bytes.len();
        let ids_at = HEADER_SIZE;
        let lengths_at = section(ids_at, entries, 12, limit)?;
        let seeds_at = section(lengths_at, entries, 10, limit)?;
        let without_at = section(seeds_at, seeds as usize, 4, limit)?;
        let blocks_offset = section(without_at, without_hash as usize, 4, limit)?;
        let methods_offset = section(blocks_offset, block_count, BLOCK_ENTRY_SIZE, limit)?;
        let mut signature_at = section(
            methods_offset,
            method_count,
            method_name_length as usize,
            limit,
        )?;
        if flags & FLAG_SIGNED != 0 {
            let hash_size = section(signature_at, 1, 4, limit).map(|at| u32_at(bytes, at - 4))?;
            let after_hashes = section(signature_at + 4, 2, hash_size as usize, limit)?;
            signature_at = section(after_hashes, block_count, 20, limit)?;
        }
        let metas_offset = section(signature_at, 1, directory_index_size as usize, limit)?;
        section(metas_offset, entries, META_ENTRY_SIZE, limit)?;
        let chunk_ids = (0..entries)
            .map(|i| {
                bytes[ids_at + i * 12..ids_at + i * 12 + 12]
                    .try_into()
                    .expect("bounds checked")
            })
            .collect();
        let offset_lengths = (0..entries)
            .map(|i| {
                let entry = &bytes[lengths_at + i * 10..lengths_at + i * 10 + 10];
                let big = |part: &[u8]| part.iter().fold(0u64, |n, b| n << 8 | u64::from(*b));
                (big(&entry[..5]), big(&entry[5..]))
            })
            .collect();
        let blocks = (0..block_count)
            .map(|i| {
                let at = blocks_offset + i * BLOCK_ENTRY_SIZE;
                let mut raw = [0u8; 16];
                raw[..BLOCK_ENTRY_SIZE].copy_from_slice(&bytes[at..at + BLOCK_ENTRY_SIZE]);
                let value = u128::from_le_bytes(raw);
                Block {
                    offset: (value & ((1 << 40) - 1)) as u64,
                    compressed: ((value >> 40) & 0xFF_FFFF) as u32,
                    uncompressed: ((value >> 64) & 0xFF_FFFF) as u32,
                    method: (value >> 88) as u8,
                }
            })
            .collect();
        let method_names = (0..method_count)
            .map(|i| {
                let at = methods_offset + i * method_name_length as usize;
                let field = &bytes[at..at + method_name_length as usize];
                let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
                String::from_utf8_lossy(&field[..end]).into_owned()
            })
            .collect();
        let meta_flags = (0..entries)
            .map(|i| bytes[metas_offset + i * META_ENTRY_SIZE + 20])
            .collect();
        Ok(Self {
            flags,
            block_size,
            partition_count,
            directory_index_size,
            perfect_hash_seeds: seeds,
            chunks_without_perfect_hash: without_hash,
            block_entry_size,
            method_name_length,
            method_names,
            chunk_ids,
            offset_lengths,
            blocks,
            meta_flags,
            blocks_offset,
            methods_offset,
            metas_offset,
        })
    }

    pub fn chunk_type(&self, chunk: usize) -> u8 {
        self.chunk_ids[chunk][11]
    }

    /// Blocks that hold a chunk, from its uncompressed offset and length.
    pub fn chunk_blocks(&self, chunk: usize) -> Range<usize> {
        let (offset, length) = self.offset_lengths[chunk];
        if length == 0 {
            return 0..0;
        }
        let size = u64::from(self.block_size);
        (offset / size) as usize..((offset + length - 1) / size + 1) as usize
    }

    pub fn is_compressed(&self) -> bool {
        self.flags & FLAG_COMPRESSED != 0
            || !self.method_names.is_empty()
            || self.blocks.iter().any(|b| b.method != 0)
    }

    /// The blocks are raw and fill a `.ucas` of `ucas_len` bytes end to end, every chunk
    /// lies inside the blocks, and the method name length is the one the writer restores.
    pub fn is_raw_and_contiguous(&self, ucas_len: u64) -> bool {
        if self.method_name_length as usize != METHOD_NAME_LENGTH {
            return false;
        }
        let mut position = 0u64;
        for block in &self.blocks {
            if block.method != 0
                || block.compressed != block.uncompressed
                || block.offset != position
            {
                return false;
            }
            position += u64::from(block.compressed);
        }
        position == ucas_len
            && (0..self.chunk_ids.len()).all(|chunk| {
                let (offset, length) = self.offset_lengths[chunk];
                let range = self.chunk_blocks(chunk);
                length == 0
                    || (offset % u64::from(self.block_size) == 0 && range.end <= self.blocks.len())
            })
    }

    pub fn has_memory_mapped_chunk(&self) -> bool {
        self.meta_flags.iter().any(|f| f & META_MEMORY_MAPPED != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compressor_test_fixtures::iostore::{self, Options};

    fn built(options: &Options) -> Vec<u8> {
        let chunks = [
            iostore::Chunk {
                kind: 2,
                data: iostore::sample_bytes(1, 10_000),
            },
            iostore::Chunk {
                kind: 9,
                data: iostore::sample_bytes(2, 4096),
            },
        ];
        iostore::build(&chunks, options).utoc
    }

    #[test]
    fn parses_a_raw_container() {
        let fixture = iostore::raw_container();
        let toc = Toc::parse(&fixture.utoc).unwrap();
        assert_eq!(toc.flags, FLAG_INDEXED);
        assert!(!toc.is_compressed());
        assert_eq!(toc.chunk_ids.len(), 5);
        assert_eq!(toc.chunk_type(2), 9);
        assert_eq!(toc.chunk_blocks(0), 0..3);
        assert_eq!(toc.chunk_blocks(1), 3..4);
        assert_eq!(toc.blocks.len(), 3 + 1 + 3 + 1 + 2);
        assert!(toc.is_raw_and_contiguous(fixture.ucas.len() as u64));
        assert!(!toc.is_raw_and_contiguous(fixture.ucas.len() as u64 + 1));
        assert_eq!(toc.meta_flags, vec![0; 5]);
    }

    #[test]
    fn reads_compressed_encrypted_and_signed_headers() {
        let options = Options {
            flags: iostore::FLAG_INDEXED | iostore::FLAG_COMPRESSED,
            method_names: vec!["Oodle".into()],
            ..Options::default()
        };
        let toc = Toc::parse(&built(&options)).unwrap();
        assert!(toc.is_compressed());
        assert_eq!(toc.method_names, vec!["Oodle"]);
        assert!(toc.blocks.iter().any(|b| b.method == 1));
        let encrypted = Options {
            flags: iostore::FLAG_ENCRYPTED,
            ..Options::default()
        };
        assert_ne!(
            Toc::parse(&built(&encrypted)).unwrap().flags & FLAG_ENCRYPTED,
            0
        );
        let signed = Options {
            flags: iostore::FLAG_SIGNED | iostore::FLAG_INDEXED,
            ..Options::default()
        };
        let toc = Toc::parse(&built(&signed)).unwrap();
        assert_ne!(toc.flags & FLAG_SIGNED, 0);
        assert_eq!(
            toc.chunk_ids.len(),
            2,
            "sections after the signature still parse"
        );
        assert_eq!(toc.meta_flags.len(), 2);
    }

    #[test]
    fn reports_version_partitions_and_memory_mapped_chunks() {
        let old = Options {
            version: 3,
            ..Options::default()
        };
        assert_eq!(
            Toc::parse(&built(&old)).unwrap_err(),
            TocError::UnsupportedVersion(3)
        );
        let split = Options {
            partitions: 2,
            ..Options::default()
        };
        assert_eq!(Toc::parse(&built(&split)).unwrap().partition_count, 2);
        let mapped = Options {
            memory_mapped_chunk: Some(1),
            ..Options::default()
        };
        assert!(Toc::parse(&built(&mapped))
            .unwrap()
            .has_memory_mapped_chunk());
    }

    #[test]
    fn rejects_short_and_bad_magic_inputs() {
        let good = built(&Options::default());
        assert!(matches!(
            Toc::parse(&good[..10]),
            Err(TocError::Unreadable(_))
        ));
        assert!(matches!(
            Toc::parse(&good[..100]),
            Err(TocError::Unreadable(_))
        ));
        let mut bad = good.clone();
        bad[0] = b'X';
        assert!(matches!(Toc::parse(&bad), Err(TocError::Unreadable(_))));
        // Every truncation inside the sections is refused, never a panic.
        for len in (0..good.len()).step_by(7) {
            let _ = Toc::parse(&good[..len]);
        }
        assert!(matches!(
            Toc::parse(&good[..good.len() - 1]),
            Err(TocError::Unreadable(_))
        ));
        let mut huge = good;
        huge[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(Toc::parse(&huge), Err(TocError::Unreadable(_))));
    }

    #[test]
    fn detects_gaps_between_blocks() {
        let gap = Options {
            gap_before_block: Some(2),
            ..Options::default()
        };
        let chunks = [iostore::Chunk {
            kind: 2,
            data: iostore::sample_bytes(1, 10_000),
        }];
        let built = iostore::build(&chunks, &gap);
        let toc = Toc::parse(&built.utoc).unwrap();
        assert!(!toc.is_raw_and_contiguous(built.ucas.len() as u64));
    }

    fn example(relative: &str) -> Option<Vec<u8>> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../example_games");
        let dir = std::fs::read_dir(&root).ok()?.flatten().find(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(relative.split('/').next().unwrap())
        })?;
        std::fs::read(dir.path().join(relative.split_once('/')?.1)).ok()
    }

    #[test]
    fn reads_headers_of_both_example_games() {
        if let Some(bytes) =
            example("UNREAL_2/Game/EscapeFrom/Content/Paks/EscapeFrom-Windows.utoc")
        {
            let toc = Toc::parse(&bytes).unwrap();
            assert_eq!((toc.chunk_ids.len(), toc.blocks.len()), (14622, 94416));
            assert_eq!((toc.flags, toc.block_size), (FLAG_INDEXED, 65536));
            assert!(toc.method_names.is_empty() && toc.perfect_hash_seeds == 0);
        }
        if let Some(bytes) = example("UNREAL_1/Windows/prealpha/Content/Paks/prealpha-Windows.utoc")
        {
            let toc = Toc::parse(&bytes).unwrap();
            assert_eq!((toc.chunk_ids.len(), toc.blocks.len()), (9335, 134933));
            assert_eq!(toc.flags, FLAG_INDEXED | FLAG_COMPRESSED);
            assert_eq!(toc.method_names, vec!["Oodle"]);
            assert_eq!(toc.perfect_hash_seeds, 4668);
            assert!(toc.is_compressed());
        }
    }
}
