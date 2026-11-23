//! After an import: how much of a source is already in the library, and cleaning it up.
//!
//! "In the library" means byte-identical content exists somewhere in the library folder, proven
//! by a full BLAKE3 hash. Clean-up trashes only such files, re-checking both the source file and
//! its library copy right before each one. Files not in the library are never touched.
//! Clean-up is always its own step, separate from importing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use rusqlite::params;
use serde::Serialize;

use crate::exclude::{ignore_file_in_use, is_ignore_file};
use crate::import::{key_of, Entry, ImportProgress, ImportProgressFn, Key};
use crate::library::Role;
use crate::{db, hash, scan, trash, Library, Source};

#[derive(Serialize, Clone, Debug, Default)]
pub struct Coverage {
    pub source_id: i64,
    pub folder_id: i64,
    pub files: i64,
    pub in_library: i64,
    pub not_in_library: i64,
    pub bytes_in_library: i64,
    pub bytes_not_in_library: i64,
    pub checked_at: i64,
}

/// A source file and where the library already has it.
#[derive(Serialize, Clone, Debug)]
pub struct Match {
    pub path: String,
    pub size: i64,
    pub library_path: String,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct CleanupResult {
    pub batch_id: i64,
    pub trashed: u64,
    pub bytes: u64,
    /// Not trashed because something no longer checked out (changed, missing library copy).
    pub skipped: u64,
    pub folders_removed: u64,
    /// Ignore files left in place because their folder still holds excluded files.
    pub kept_ignore_files: u64,
    pub errors: Vec<String>,
    pub cancelled: bool,
}

struct Compared {
    source: Source,
    folder_rel: String,
    entries: Vec<Entry>,
    incoming: usize,
    /// incoming index → library index with the same content.
    matches: HashMap<usize, usize>,
}

impl Library {
    /// Compares every file under `folder_id` (in an import or indexed source) with the library.
    fn compare_with_library(
        &self,
        folder_id: i64,
        progress: ImportProgressFn,
        cancel: &AtomicBool,
    ) -> Result<Compared> {
        let lib_src = self
            .library_source()?
            .context("set this library's folder first")?;
        let folder = self.folder(folder_id)?;
        let source = self.source(folder.source_id)?;
        if source.role == Role::Library {
            bail!("that folder is in the library itself");
        }
        if !source.online {
            bail!("{} is offline", source.root);
        }
        let mut entries = self.files_under(&source, &folder.rel_path, false)?;
        let incoming = entries.len();
        entries.extend(self.files_under(&lib_src, "", true)?);
        self.hash_colliding(&mut entries, progress, cancel)?;
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let mut lib_by_key: HashMap<Key, usize> = HashMap::new();
        for i in incoming..entries.len() {
            if let k @ Key::Full(_) = key_of(&entries, i) {
                lib_by_key.entry(k).or_insert(i);
            }
        }
        let mut matches = HashMap::new();
        for i in 0..incoming {
            if let Some(&li) = lib_by_key.get(&key_of(&entries, i)) {
                matches.insert(i, li);
            }
        }
        Ok(Compared {
            source,
            folder_rel: folder.rel_path,
            entries,
            incoming,
            matches,
        })
    }

    /// How much of a folder (a whole source when it is the root) is already in the library.
    /// The result for a source root is remembered and shown with the source.
    pub fn check_coverage(
        &self,
        folder_id: i64,
        progress: ImportProgressFn,
        cancel: &AtomicBool,
    ) -> Result<Coverage> {
        let c = self.compare_with_library(folder_id, progress, cancel)?;
        let mut cov = Coverage {
            source_id: c.source.id,
            folder_id,
            files: c.incoming as i64,
            checked_at: db::now_secs(),
            ..Default::default()
        };
        for i in 0..c.incoming {
            let size = c.entries[i].size;
            if c.matches.contains_key(&i) {
                cov.in_library += 1;
                cov.bytes_in_library += size;
            } else {
                cov.not_in_library += 1;
                cov.bytes_not_in_library += size;
            }
        }
        if c.folder_rel.is_empty() {
            self.writer().execute(
                "INSERT INTO source_coverage (source_id, checked_at, files, in_library, not_in_library,
                                              bytes_in_library, bytes_not_in_library)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (source_id) DO UPDATE SET
                   checked_at = excluded.checked_at, files = excluded.files, in_library = excluded.in_library,
                   not_in_library = excluded.not_in_library, bytes_in_library = excluded.bytes_in_library,
                   bytes_not_in_library = excluded.bytes_not_in_library",
                params![
                    cov.source_id,
                    cov.checked_at,
                    cov.files,
                    cov.in_library,
                    cov.not_in_library,
                    cov.bytes_in_library,
                    cov.bytes_not_in_library
                ],
            )?;
        }
        Ok(cov)
    }

    /// The last remembered coverage check for a source.
    pub fn coverage(&self, source_id: i64) -> Result<Option<Coverage>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .reader()?
            .query_row(
                "SELECT c.source_id, d.id, c.files, c.in_library, c.not_in_library, c.bytes_in_library,
                        c.bytes_not_in_library, c.checked_at
                   FROM source_coverage c JOIN folder d ON d.source_id = c.source_id AND d.rel_path = ''
                  WHERE c.source_id = ?1",
                [source_id],
                |r| {
                    Ok(Coverage {
                        source_id: r.get(0)?,
                        folder_id: r.get(1)?,
                        files: r.get(2)?,
                        in_library: r.get(3)?,
                        not_in_library: r.get(4)?,
                        bytes_in_library: r.get(5)?,
                        bytes_not_in_library: r.get(6)?,
                        checked_at: r.get(7)?,
                    })
                },
            )
            .optional()?)
    }

    /// Files under `folder_id` that are (`in_library = true`) or are not in the library.
    pub fn coverage_files(
        &self,
        folder_id: i64,
        in_library: bool,
        limit: usize,
        progress: ImportProgressFn,
        cancel: &AtomicBool,
    ) -> Result<Vec<Match>> {
        let c = self.compare_with_library(folder_id, progress, cancel)?;
        let mut out = Vec::new();
        for i in 0..c.incoming {
            let m = c.matches.get(&i);
            if m.is_some() != in_library {
                continue;
            }
            out.push(Match {
                path: c.entries[i].abs.to_string_lossy().to_string(),
                size: c.entries[i].size,
                library_path: m
                    .map(|&li| c.entries[li].abs.to_string_lossy().to_string())
                    .unwrap_or_default(),
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Trashes every file under `folder_id` whose content is verified to be in the library, then
    /// removes folders that became empty. One undoable batch.
    pub fn cleanup(
        &self,
        folder_id: i64,
        progress: ImportProgressFn,
        cancel: &AtomicBool,
    ) -> Result<CleanupResult> {
        let c = self.compare_with_library(folder_id, progress, cancel)?;
        let root = PathBuf::from(&c.source.root);
        let batch_id = {
            let conn = self.writer();
            conn.execute(
                "INSERT INTO batch (kind, created_at, description, scope_source_id, scope_rel)
                 VALUES ('cleanup', ?1, ?2, ?3, ?4)",
                params![
                    db::now_secs(),
                    format!(
                        "Clean up {} ({} files already in the library)",
                        crate::library::join_rel(&root, &c.folder_rel).display(),
                        c.matches.len()
                    ),
                    c.source.id,
                    c.folder_rel
                ],
            )?;
            conn.last_insert_rowid()
        };
        let mut res = CleanupResult {
            batch_id,
            ..Default::default()
        };
        let mut order: Vec<(usize, usize)> = c.matches.iter().map(|(&a, &b)| (a, b)).collect();
        // Ignore files last: they go only if nothing else is left in their folder.
        order.sort_unstable_by_key(|&(i, li)| (is_ignore_file(&c.entries[i].abs), i, li));
        let total = order.len() as u64;
        let bytes_total: u64 = order.iter().map(|(i, _)| c.entries[*i].size as u64).sum();
        let mut bytes_done = 0u64;
        let mut touched_dirs: Vec<PathBuf> = Vec::new();
        for (n, (i, li)) in order.into_iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                res.cancelled = true;
                break;
            }
            progress(&ImportProgress {
                phase: "cleaning",
                done: n as u64,
                total,
                bytes_done,
                bytes_total,
            });
            let src = &c.entries[i];
            let lib = &c.entries[li];
            bytes_done += src.size as u64;
            if ignore_file_in_use(&src.abs) {
                res.kept_ignore_files += 1;
                continue;
            }
            let verified = (|| -> Result<()> {
                let want = lib.full.context("no hash for the library copy")?;
                let md = std::fs::metadata(&src.abs)
                    .with_context(|| format!("{} is gone", src.abs.display()))?;
                if md.len() as i64 != src.size || crate::fsmeta::mtime_ns(&md) != src.mtime_ns {
                    bail!("{} changed since it was indexed", src.abs.display());
                }
                if src.size > 0 && hash::full_hash(&src.abs)? != want {
                    bail!("{} no longer matches the library copy", src.abs.display());
                }
                if !lib.abs.is_file() {
                    bail!("the library copy {} is gone", lib.abs.display());
                }
                if lib.size > 0 && hash::full_hash(&lib.abs)? != want {
                    bail!("the library copy {} changed", lib.abs.display());
                }
                Ok(())
            })();
            if let Err(e) = verified {
                res.skipped += 1;
                if res.errors.len() < 20 {
                    res.errors.push(format!("skipped: {e:#}"));
                }
                continue;
            }
            match trash::trash(&src.abs) {
                Ok(at) => {
                    self.journal(
                        batch_id,
                        "trash",
                        Some(&src.abs),
                        at.as_deref(),
                        src.size,
                        src.full.as_ref(),
                        None,
                    )?;
                    res.trashed += 1;
                    res.bytes += src.size as u64;
                    if let Some(p) = src.abs.parent() {
                        touched_dirs.push(p.to_path_buf());
                    }
                }
                Err(e) => {
                    res.skipped += 1;
                    if res.errors.len() < 20 {
                        res.errors.push(format!("{}: {e:#}", src.abs.display()));
                    }
                }
            }
        }
        // Folders left empty (deepest first). Never the selected folder or the source root.
        let stop = crate::library::join_rel(&root, &c.folder_rel);
        touched_dirs.sort();
        touched_dirs.dedup();
        let mut candidates: Vec<PathBuf> = Vec::new();
        for d in touched_dirs {
            let mut cur = Some(d.as_path());
            while let Some(dir) = cur {
                if dir == stop || !dir.starts_with(&stop) {
                    break;
                }
                candidates.push(dir.to_path_buf());
                cur = dir.parent();
            }
        }
        candidates.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        candidates.dedup();
        for dir in candidates {
            if is_empty_dir(&dir) && std::fs::remove_dir(&dir).is_ok() {
                self.journal(batch_id, "rmdir", None, Some(&dir), 0, None, None)?;
                res.folders_removed += 1;
            }
        }
        let state = if res.skipped == 0 && !res.cancelled {
            "done"
        } else {
            "partial"
        };
        self.writer().execute(
            "UPDATE batch SET state = ?2 WHERE id = ?1",
            params![batch_id, state],
        )?;
        scan::scan_folder(
            self,
            c.source.id,
            &c.folder_rel,
            &|_| {},
            &AtomicBool::new(false),
        )?;
        if c.folder_rel.is_empty() {
            // The remembered coverage is stale now.
            self.writer().execute(
                "DELETE FROM source_coverage WHERE source_id = ?1",
                [c.source.id],
            )?;
        }
        Ok(res)
    }
}

fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut it| it.next().is_none())
        .unwrap_or(false)
}
