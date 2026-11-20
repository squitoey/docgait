//! Low-level file operations used by import, clean-up and undo. Nothing here deletes
//! permanently: removal goes through the OS trash (`trash.rs`).

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Copies `from` to `to` (which must not exist), keeping the modified time, and proves the copy is
/// byte-identical by hashing the source while reading it and re-reading the copy. Writes to a
/// temporary name next to `to` first, so a crash never leaves a half-written file under the real
/// name. Returns the content hash.
pub fn copy_verified(from: &Path, to: &Path) -> Result<[u8; 32]> {
    if to.exists() {
        bail!("{} already exists", to.display());
    }
    let parent = to.parent().context("destination has no folder")?;
    fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    let tmp = temp_sibling(to);
    let result = (|| -> Result<[u8; 32]> {
        let mut src =
            File::open(from).with_context(|| format!("cannot read {}", from.display()))?;
        let mut dst =
            File::create(&tmp).with_context(|| format!("cannot write in {}", parent.display()))?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            dst.write_all(&buf[..n])?;
        }
        dst.sync_all()?;
        let md = src.metadata()?;
        if let Ok(mtime) = md.modified() {
            let _ = dst.set_modified(mtime);
        }
        drop(dst);
        let source_hash = *hasher.finalize().as_bytes();
        let copy_hash = crate::hash::full_hash(&tmp)?;
        if copy_hash != source_hash {
            bail!("the copy of {} does not match the original", from.display());
        }
        // Last check right before the rename: never replace something that appeared meanwhile.
        if to.exists() {
            bail!("{} appeared while copying", to.display());
        }
        fs::rename(&tmp, to).with_context(|| format!("cannot rename into {}", to.display()))?;
        Ok(source_hash)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Moves a file, across volumes if needed (copy, check, then remove the original).
pub fn move_file(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        bail!("{} already exists", to.display());
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_verified(from, to)?;
    fs::remove_file(from)?;
    Ok(())
}

/// Renames within a folder, refusing to replace anything.
pub fn rename_no_replace(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        bail!("{} already exists", to.display());
    }
    fs::rename(from, to)
        .with_context(|| format!("cannot rename {} to {}", from.display(), to.display()))
}

fn temp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    path.with_file_name(format!(
        ".{name}.docgait-tmp-{}-{nanos}",
        std::process::id()
    ))
}

/// Whether file names on this platform's default filesystems ignore case (APFS, NTFS).
pub fn case_insensitive_names() -> bool {
    cfg!(any(target_os = "macos", windows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_is_verified_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        fs::write(&a, b"hello").unwrap();
        let b = dir.path().join("sub/b.txt");
        let h = copy_verified(&a, &b).unwrap();
        assert_eq!(fs::read(&b).unwrap(), b"hello");
        assert_eq!(h, crate::hash::full_hash(&a).unwrap());
        assert_eq!(
            fs::metadata(&a).unwrap().modified().unwrap(),
            fs::metadata(&b).unwrap().modified().unwrap()
        );
        assert!(copy_verified(&a, &b).is_err());
        // No temporary files left behind.
        let left: Vec<_> = fs::read_dir(dir.path().join("sub")).unwrap().collect();
        assert_eq!(left.len(), 1);
    }
}
