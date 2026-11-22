//! Content hashes (BLAKE3): a cheap partial hash to rule out most same-size files, and the full
//! hash that proves two files are byte-identical.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::Result;

pub const PARTIAL_CHUNK: u64 = 64 * 1024;

/// Files at or below this size skip the partial hash and go straight to a full hash.
pub const SMALL: u64 = 2 * PARTIAL_CHUNK;

/// Hash of the first and last 64 KB plus the size. Only meaningful above `SMALL`.
pub fn partial_hash(path: &Path, size: u64) -> Result<[u8; 32]> {
    let mut f = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; PARTIAL_CHUNK as usize];
    f.read_exact(&mut buf)?;
    hasher.update(&buf);
    f.seek(SeekFrom::Start(size - PARTIAL_CHUNK))?;
    f.read_exact(&mut buf)?;
    hasher.update(&buf);
    hasher.update(&size.to_le_bytes());
    Ok(*hasher.finalize().as_bytes())
}

pub fn full_hash(path: &Path) -> Result<[u8; 32]> {
    let mut f = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}
