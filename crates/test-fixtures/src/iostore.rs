//! Synthetic IoStore containers (`.utoc` and `.ucas`) for tests.

/// One chunk of a synthetic container: its type byte and uncompressed bytes.
#[derive(Clone)]
pub struct Chunk {
    pub kind: u8,
    pub data: Vec<u8>,
}

/// Header and layout options for [`build`]; the defaults give a raw version 8 container.
#[derive(Clone)]
pub struct Options {
    pub version: u8,
    pub block_size: u32,
    pub flags: u8,
    pub partitions: u32,
    pub directory_index: Vec<u8>,
    /// Zero bytes inserted before the block at this index, to create a gap.
    pub gap_before_block: Option<usize>,
    /// Compression method names; when non-empty every second block uses method 1.
    pub method_names: Vec<String>,
    pub memory_mapped_chunk: Option<usize>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            version: 8,
            block_size: 4096,
            flags: FLAG_INDEXED,
            partitions: 1,
            directory_index: vec![0xD1; 37],
            gap_before_block: None,
            method_names: Vec::new(),
            memory_mapped_chunk: None,
        }
    }
}

pub const FLAG_COMPRESSED: u8 = 1;
pub const FLAG_ENCRYPTED: u8 = 2;
pub const FLAG_SIGNED: u8 = 4;
pub const FLAG_INDEXED: u8 = 8;
pub const FLAG_ON_DEMAND: u8 = 16;
pub const MAGIC: &[u8; 16] = b"-==--==--==--==-";
pub const HEADER_SIZE: usize = 144;
pub const METHOD_NAME_LENGTH: usize = 32;

/// A built container.
pub struct Built {
    pub utoc: Vec<u8>,
    pub ucas: Vec<u8>,
}

fn put_u32(out: &mut [u8], at: usize, value: u32) {
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Deterministic, compressible bytes: runs that depend on `seed`.
pub fn sample_bytes(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_add((i / 97) as u8 % 5))
        .collect()
}

/// Build a container. Chunks start on block boundaries, as in Unreal's writer.
pub fn build(chunks: &[Chunk], options: &Options) -> Built {
    let block_size = options.block_size as usize;
    let mut ucas = Vec::new();
    let mut blocks = Vec::new(); // (offset, compressed, uncompressed, method)
    let mut offset_lengths = Vec::new();
    let mut uncompressed_offset = 0u64;
    for chunk in chunks {
        offset_lengths.push((uncompressed_offset, chunk.data.len() as u64));
        for piece in chunk.data.chunks(block_size) {
            if options.gap_before_block == Some(blocks.len()) {
                ucas.extend([0u8; 16]);
            }
            let method = u8::from(!options.method_names.is_empty() && blocks.len() % 2 == 1);
            blocks.push((ucas.len() as u64, piece.len(), piece.len(), method));
            ucas.extend(piece);
        }
        uncompressed_offset += chunk.data.len().div_ceil(block_size) as u64 * block_size as u64;
    }
    let count = chunks.len();
    let mut utoc = vec![0u8; HEADER_SIZE];
    utoc[..16].copy_from_slice(MAGIC);
    utoc[16] = options.version;
    put_u32(&mut utoc, 20, HEADER_SIZE as u32);
    put_u32(&mut utoc, 24, count as u32);
    put_u32(&mut utoc, 28, blocks.len() as u32);
    put_u32(&mut utoc, 32, 12);
    put_u32(&mut utoc, 36, options.method_names.len() as u32);
    put_u32(&mut utoc, 40, METHOD_NAME_LENGTH as u32);
    put_u32(&mut utoc, 44, options.block_size);
    put_u32(&mut utoc, 48, options.directory_index.len() as u32);
    put_u32(&mut utoc, 52, options.partitions);
    utoc[56..64].copy_from_slice(&0xABCDu64.to_le_bytes());
    utoc[80] = options.flags;
    utoc[88..96].copy_from_slice(&u64::MAX.to_le_bytes());
    for (index, chunk) in chunks.iter().enumerate() {
        let mut id = [0u8; 12];
        id[..8].copy_from_slice(&(index as u64 + 1).to_le_bytes());
        id[11] = chunk.kind;
        utoc.extend(id);
    }
    for (offset, length) in &offset_lengths {
        utoc.extend(&offset.to_be_bytes()[3..]);
        utoc.extend(&length.to_be_bytes()[3..]);
    }
    for (offset, compressed, uncompressed, method) in &blocks {
        let packed = u128::from(*offset)
            | (*compressed as u128) << 40
            | (*uncompressed as u128) << 64
            | u128::from(*method) << 88;
        utoc.extend(&packed.to_le_bytes()[..12]);
    }
    for name in &options.method_names {
        let mut field = name.as_bytes().to_vec();
        field.resize(METHOD_NAME_LENGTH, 0);
        utoc.extend(field);
    }
    if options.flags & FLAG_SIGNED != 0 {
        utoc.extend(20u32.to_le_bytes());
        utoc.extend(vec![0x51u8; 40]);
        utoc.extend(vec![0x52u8; blocks.len() * 20]);
    }
    utoc.extend(&options.directory_index);
    for (index, chunk) in chunks.iter().enumerate() {
        let hash = stub_hash(&chunk.data);
        utoc.extend(hash);
        let flag = u8::from(options.memory_mapped_chunk == Some(index)) * 2;
        utoc.extend([flag, 0, 0, 0]);
    }
    Built { utoc, ucas }
}

/// A stand-in 20-byte chunk hash so this crate needs no hashing dependency.
fn stub_hash(data: &[u8]) -> [u8; 20] {
    let mut out = [0u8; 20];
    for (i, byte) in data.iter().enumerate() {
        out[i % 20] = out[i % 20].wrapping_mul(31).wrapping_add(*byte);
    }
    out
}

/// A raw container with a mix of compressible bulk data, shader code, and a header chunk.
pub fn raw_container() -> Built {
    build(
        &[
            Chunk {
                kind: 2,
                data: sample_bytes(1, 10_000),
            },
            Chunk {
                kind: 1,
                data: sample_bytes(9, 4096),
            },
            Chunk {
                kind: 9,
                data: sample_bytes(3, 9000),
            },
            Chunk {
                kind: 6,
                data: sample_bytes(4, 100),
            },
            Chunk {
                kind: 2,
                data: (0..5000u32)
                    .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
                    .collect(),
            },
        ],
        &Options::default(),
    )
}
