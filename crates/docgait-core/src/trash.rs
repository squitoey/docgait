//! Move a file to the system trash, and bring it back.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// `DOCGAIT_TRASH_DIR` replaces the OS trash with a plain folder (tests, experiments).
fn override_dir() -> Option<PathBuf> {
    std::env::var_os("DOCGAIT_TRASH_DIR").map(PathBuf::from)
}

/// Moves into the override folder under a unique name.
fn trash_to(dir: &Path, path: &Path) -> Result<Option<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut dest = dir.join(&name);
    let mut n = 1;
    while dest.exists() {
        n += 1;
        dest = dir.join(format!("{n}-{name}"));
    }
    crate::fsops::move_file(path, &dest)?;
    Ok(Some(dest))
}

/// Moves `path` to the trash. Returns where it ended up when the system reports that
/// (macOS); elsewhere the trash is asked for the file again at restore time.
#[cfg(target_os = "macos")]
pub fn trash(path: &Path) -> Result<Option<PathBuf>> {
    if let Some(dir) = override_dir() {
        return trash_to(&dir, path);
    }
    use anyhow::{anyhow, Context};
    use objc2::rc::Retained;
    use objc2_foundation::{NSFileManager, NSString, NSURL};

    let ns_path = NSString::from_str(&path.to_string_lossy());
    let url = NSURL::fileURLWithPath(&ns_path);
    let fm = NSFileManager::defaultManager();
    let mut out: Option<Retained<NSURL>> = None;
    fm.trashItemAtURL_resultingItemURL_error(&url, Some(&mut out))
        .map_err(|e| anyhow!("{}", e.localizedDescription()))?;
    let out = out.context("the system did not report where the file went")?;
    let p = out.path().context("trash location has no path")?;
    Ok(Some(PathBuf::from(p.to_string())))
}

/// Puts a trashed file back at `original`.
#[cfg(target_os = "macos")]
pub fn restore(original: &Path, trashed_at: Option<&Path>) -> Result<()> {
    use anyhow::{bail, Context};
    let at = trashed_at.context("no record of where the file went")?;
    if !at.exists() {
        bail!(
            "file is no longer at {} (was the trash emptied?)",
            at.display()
        );
    }
    crate::fsops::move_file(at, original)
}

/// Moves `path` to the trash (Recycle Bin on Windows, the freedesktop trash on Linux).
#[cfg(not(target_os = "macos"))]
pub fn trash(path: &Path) -> Result<Option<PathBuf>> {
    if let Some(dir) = override_dir() {
        return trash_to(&dir, path);
    }
    ::trash::delete(path).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(None)
}

/// Puts a trashed file back at `original`, taking the most recently trashed match.
#[cfg(not(target_os = "macos"))]
pub fn restore(original: &Path, trashed_at: Option<&Path>) -> Result<()> {
    if override_dir().is_some() {
        let at = trashed_at.ok_or_else(|| anyhow::anyhow!("no record of where the file went"))?;
        return crate::fsops::move_file(at, original);
    }
    use anyhow::{anyhow, Context};
    let item = ::trash::os_limited::list()
        .map_err(|e| anyhow!("cannot read the trash: {e}"))?
        .into_iter()
        .filter(|i| i.original_path() == original)
        .max_by_key(|i| i.time_deleted)
        .context("file is no longer in the trash (was it emptied?)")?;
    ::trash::os_limited::restore_all([item]).map_err(|e| anyhow!("{e}"))
}
