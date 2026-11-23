//! Import into the library: merge one or more source folders into the library folder, skipping
//! anything the library already has, bringing repeated files in once, and keeping every
//! conflicting variant as a `.conflict-` file next to the original.
//!
//! Three steps, each safe to stop and repeat:
//! 1. **plan** (`plan_import`): hash what needs hashing, decide every file's fate, store the plan.
//!    Nothing on disk changes.
//! 2. **execute** (`execute_import`): copy (never move) with verification, journal each step.
//!    An interrupted import resumes where it stopped.
//! 3. **undo** (`undo_batch`): walk the journal backwards; copies go to the trash, renames are
//!    reversed, folders created by the import are removed if empty.
//!
//! Sources are never modified. Removing originals is a separate clean-up action.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::library::{join_rel, Role};
use crate::{db, fsops, hash, scan, trash, IndexLevel, Library, Source};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Layout {
    /// Combine relative paths: CopyA/Taxes/W2.pdf and CopyB/Taxes/W2.pdf both map to Taxes/W2.pdf.
    Merge,
    /// Each selected folder gets its own folder in the destination.
    OwnFolder,
}

impl Layout {
    fn as_str(self) -> &'static str {
        match self {
            Layout::Merge => "merge",
            Layout::OwnFolder => "own-folder",
        }
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct ImportProgress {
    /// "hashing", "copying" or "finishing"
    pub phase: &'static str,
    pub done: u64,
    pub total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

pub type ImportProgressFn<'a> = &'a (dyn Fn(&ImportProgress) + Sync);

#[derive(Serialize, Clone, Debug, Default)]
pub struct ImportPlan {
    pub run_id: i64,
    pub state: String,
    /// The destination folder (absolute).
    pub dest_path: String,
    pub files: i64,
    pub to_copy: i64,
    pub bytes_to_copy: i64,
    pub skipped_in_library: i64,
    pub skipped_duplicate: i64,
    pub conflicts: i64,
    pub needs_review: i64,
    pub batch_id: Option<i64>,
}

#[derive(Serialize, Clone, Debug)]
pub struct ImportItem {
    pub id: i64,
    pub action: String,
    pub from_path: String,
    pub to_path: Option<String>,
    pub size: i64,
    pub mtime_ns: i64,
    pub note: Option<String>,
    pub conflict_id: Option<i64>,
    pub state: String,
    pub error: Option<String>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct ImportResult {
    pub run_id: i64,
    pub batch_id: i64,
    pub copied: u64,
    pub renamed: u64,
    pub failed: u64,
    pub bytes: u64,
    pub errors: Vec<String>,
    pub cancelled: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct BatchInfo {
    pub id: i64,
    pub kind: String,
    pub created_at: i64,
    pub description: String,
    pub state: String,
    pub done: i64,
    pub undone: i64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct UndoResult {
    pub batch_id: i64,
    pub undone: u64,
    pub failed: u64,
    pub errors: Vec<String>,
}

/// id, op, from, to, content hash.
type JournalRow = (
    i64,
    String,
    Option<String>,
    Option<String>,
    Option<Vec<u8>>,
    Option<i64>,
);

/// A file taking part in planning: an incoming file or one already in the library.
pub(crate) struct Entry {
    pub(crate) file_id: i64,
    pub(crate) abs: PathBuf,
    pub(crate) size: i64,
    pub(crate) mtime_ns: i64,
    pub(crate) partial: Option<[u8; 32]>,
    pub(crate) full: Option<[u8; 32]>,
    pub(crate) in_library: bool,
    /// Incoming: path relative to the selected folder. Library: relative to the library root.
    pub(crate) rel: String,
    /// Incoming: name of the source it came from, used in conflict names.
    pub(crate) origin: String,
    /// Incoming: the selected folder's own name (for the own-folder layout).
    pub(crate) sel_name: String,
}

impl Library {
    /// The library folder of this library, if it has been set.
    pub fn library_source(&self) -> Result<Option<Source>> {
        Ok(self
            .sources()?
            .into_iter()
            .find(|s| s.role == Role::Library))
    }

    /// Makes `path` this library's folder (a source with the library role, full index level).
    pub fn set_library_folder(&self, path: &Path) -> Result<Source> {
        let root =
            dunce::canonicalize(path).with_context(|| format!("cannot open {}", path.display()))?;
        if let Some(existing) = self.library_source()? {
            if Path::new(&existing.root) == root {
                return Ok(existing);
            }
            bail!(
                "this library's folder is already {}; moving it is not supported yet",
                existing.root
            );
        }
        if let Some(s) = self
            .sources()?
            .into_iter()
            .find(|s| Path::new(&s.root) == root)
        {
            // An existing source becomes the library folder.
            self.writer().execute(
                "UPDATE source SET role = 'library', index_level = ?2 WHERE id = ?1",
                params![s.id, IndexLevel::Full.as_str()],
            )?;
            return self.source(s.id);
        }
        self.add_source(&root, Role::Library, Some(IndexLevel::Full))
    }

    /// Step 1: decide what happens to every file in `folder_ids` and store the plan. Hashes what
    /// is needed to tell copies apart (cached in the index). Nothing on disk changes.
    pub fn plan_import(
        &self,
        folder_ids: &[i64],
        dest: &str,
        layout: Layout,
        progress: ImportProgressFn,
        cancel: &AtomicBool,
    ) -> Result<ImportPlan> {
        let lib_src = self
            .library_source()?
            .context("set this library's folder first")?;
        let lib_root = PathBuf::from(&lib_src.root);
        if !lib_root.is_dir() {
            bail!("the library folder {} is not available", lib_root.display());
        }
        if folder_ids.is_empty() {
            bail!("choose at least one folder to import");
        }
        let dest = normalize_rel(dest);
        if !scan::has_scanned(self, lib_src.id)? {
            scan::scan_source(self, lib_src.id, &|_| {}, cancel)?;
        }

        // Incoming files.
        let mut entries: Vec<Entry> = Vec::new();
        for &fid in folder_ids {
            let folder = self.folder(fid)?;
            let src = self.source(folder.source_id)?;
            if src.role == Role::Library {
                bail!(
                    "{} is inside the library folder; it is already in the library",
                    src.root
                );
            }
            if !src.online {
                bail!("{} is offline", src.root);
            }
            let src_root = PathBuf::from(&src.root);
            if lib_root.starts_with(&src_root) {
                bail!(
                    "{} contains the library folder; importing it would copy the library into itself",
                    src.root
                );
            }
            let origin = src_root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "source".into());
            let sel_name = if folder.rel_path.is_empty() {
                origin.clone()
            } else {
                folder.name.clone()
            };
            entries.extend(
                self.files_under(&src, &folder.rel_path, false)?
                    .into_iter()
                    .map(|mut e| {
                        e.origin = origin.clone();
                        e.sel_name = sel_name.clone();
                        e
                    }),
            );
        }
        // Folders selected twice (or nested selections) must not import a file twice.
        let mut seen = HashSet::new();
        entries.retain(|e| seen.insert(e.file_id));
        let incoming = entries.len();
        entries.extend(self.files_under(&lib_src, "", true)?);

        self.hash_colliding(&mut entries, progress, cancel)?;
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }

        let mut plan = decide(&entries, incoming, &lib_root, &dest, layout);
        let bundles = bundle_items(&entries, &plan, &lib_root);
        plan.to_copy += bundles.len() as i64;
        plan.bytes_to_copy += bundles.iter().map(|b| b.size).sum::<i64>();
        self.store_plan(
            &lib_src, &dest, layout, folder_ids, &plan, &entries, &bundles,
        )
    }

    /// Every present file under `rel` in `src` (packages are skipped: they are folders).
    pub(crate) fn files_under(
        &self,
        src: &Source,
        rel: &str,
        in_library: bool,
    ) -> Result<Vec<Entry>> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare(
            "SELECT f.id, d.rel_path, f.name, f.size, f.mtime_ns, f.partial_hash, f.content_hash
               FROM file f JOIN folder d ON d.id = f.folder_id
              WHERE f.source_id = ?1 AND f.state = 'present' AND f.kind <> 'package'
                AND (?2 = '' OR d.rel_path = ?2 OR substr(d.rel_path, 1, length(?2) + 1) = ?2 || '/')",
        )?;
        let root = PathBuf::from(&src.root);
        let rows = stmt
            .query_map(params![src.id, rel], |r| {
                let folder_rel: String = r.get(1)?;
                let name: String = r.get(2)?;
                let to_arr =
                    |v: Option<Vec<u8>>| v.and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok());
                Ok((
                    r.get::<_, i64>(0)?,
                    folder_rel,
                    name,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    to_arr(r.get(5)?),
                    to_arr(r.get(6)?),
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .map(|(id, folder_rel, name, size, mtime, partial, full)| {
                let abs = join_rel(&root, &folder_rel).join(&name);
                let within = strip_rel(&folder_rel, rel);
                let rel_path = if within.is_empty() {
                    name
                } else {
                    format!("{within}/{name}")
                };
                Entry {
                    file_id: id,
                    abs,
                    size,
                    mtime_ns: mtime,
                    partial,
                    full,
                    in_library,
                    rel: rel_path,
                    origin: String::new(),
                    sel_name: String::new(),
                }
            })
            .collect())
    }

    /// Size → partial hash → full hash, only for files that could be copies of an incoming file.
    pub(crate) fn hash_colliding(
        &self,
        entries: &mut [Entry],
        progress: ImportProgressFn,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let mut by_size: HashMap<i64, Vec<usize>> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            by_size.entry(e.size).or_default().push(i);
        }
        let groups: Vec<Vec<usize>> = by_size
            .into_values()
            .filter(|g| g.len() > 1 && g.iter().any(|&i| !entries[i].in_library))
            .collect();

        // Partial hashes for big files in colliding size groups.
        let need_partial: Vec<usize> = groups
            .iter()
            .flatten()
            .copied()
            .filter(|&i| {
                let e = &entries[i];
                e.size as u64 > hash::SMALL && e.partial.is_none() && e.full.is_none()
            })
            .collect();
        // Full hashes: small files in colliding groups now; big ones after the partial pass.
        let mut need_full: Vec<usize> = groups
            .iter()
            .flatten()
            .copied()
            .filter(|&i| entries[i].size as u64 <= hash::SMALL && entries[i].full.is_none())
            .collect();

        let total = (need_partial.len() + need_full.len()) as u64;
        let mut done = 0u64;
        let report = |done: u64, total: u64| {
            progress(&ImportProgress {
                phase: "hashing",
                done,
                total,
                bytes_done: 0,
                bytes_total: 0,
            })
        };
        report(0, total);

        let partials = self.hash_many(entries, &need_partial, true, cancel, &mut |n| {
            done += n;
            report(done, total);
        })?;
        for (i, h) in partials {
            entries[i].partial = Some(h);
        }

        // Big files whose (size, partial) still collide with an incoming file need a full hash.
        let mut by_partial: HashMap<(i64, [u8; 32]), Vec<usize>> = HashMap::new();
        for g in &groups {
            for &i in g {
                let e = &entries[i];
                if e.size as u64 > hash::SMALL && e.full.is_none() {
                    if let Some(p) = e.partial {
                        by_partial.entry((e.size, p)).or_default().push(i);
                    }
                }
            }
        }
        // A big file with a cached full hash but no partial still has to be matched: give it its
        // own full-hash peers by size instead.
        for g in &groups {
            let has_full_big = g
                .iter()
                .any(|&i| entries[i].size as u64 > hash::SMALL && entries[i].full.is_some());
            if has_full_big {
                for &i in g {
                    if entries[i].full.is_none() && entries[i].size as u64 > hash::SMALL {
                        need_full.push(i);
                    }
                }
            }
        }
        for v in by_partial.into_values() {
            if v.len() > 1 && v.iter().any(|&i| !entries[i].in_library) {
                need_full.extend(v);
            }
        }
        need_full.sort_unstable();
        need_full.dedup();
        let total = done + need_full.len() as u64;
        let fulls = self.hash_many(entries, &need_full, false, cancel, &mut |n| {
            done += n;
            report(done, total);
        })?;
        for (i, h) in fulls {
            entries[i].full = Some(h);
        }
        Ok(())
    }

    /// Hashes `which` in parallel, in chunks, saving results to the index as it goes.
    fn hash_many(
        &self,
        entries: &[Entry],
        which: &[usize],
        partial: bool,
        cancel: &AtomicBool,
        on_chunk: &mut dyn FnMut(u64),
    ) -> Result<Vec<(usize, [u8; 32])>> {
        let mut out = Vec::with_capacity(which.len());
        for chunk in which.chunks(64) {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let results: Vec<(usize, [u8; 32])> = chunk
                .par_iter()
                .filter_map(|&i| {
                    let e = &entries[i];
                    let h = if partial {
                        hash::partial_hash(&e.abs, e.size as u64)
                    } else {
                        hash::full_hash(&e.abs)
                    };
                    h.ok().map(|h| (i, h))
                })
                .collect();
            {
                let mut conn = self.writer();
                let tx = conn.transaction()?;
                {
                    let sql = if partial {
                        "UPDATE file SET partial_hash = ?2 WHERE id = ?1 AND size = ?3 AND mtime_ns = ?4"
                    } else {
                        "UPDATE file SET content_hash = ?2 WHERE id = ?1 AND size = ?3 AND mtime_ns = ?4"
                    };
                    let mut stmt = tx.prepare_cached(sql)?;
                    for (i, h) in &results {
                        let e = &entries[*i];
                        stmt.execute(params![e.file_id, &h[..], e.size, e.mtime_ns])?;
                    }
                }
                tx.commit()?;
            }
            on_chunk(chunk.len() as u64);
            out.extend(results);
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn store_plan(
        &self,
        lib_src: &Source,
        dest: &str,
        layout: Layout,
        folder_ids: &[i64],
        plan: &Decided,
        entries: &[Entry],
        bundles: &[BundleItem],
    ) -> Result<ImportPlan> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO import_run (created_at, library_source_id, dest_rel, layout, selection_json,
                                     files, to_copy, bytes_to_copy, skipped_in_library, skipped_duplicate,
                                     conflicts, needs_review)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                db::now_secs(),
                lib_src.id,
                dest,
                layout.as_str(),
                serde_json::to_string(folder_ids)?,
                plan.files,
                plan.to_copy,
                plan.bytes_to_copy,
                plan.skipped_in_library,
                plan.skipped_duplicate,
                plan.conflicts.len() as i64,
                plan.conflicts.iter().filter(|c| c.needs_review).count() as i64,
            ],
        )?;
        let run_id = tx.last_insert_rowid();
        let mut conflict_ids = Vec::with_capacity(plan.conflicts.len());
        for c in &plan.conflicts {
            tx.execute(
                "INSERT INTO conflict (run_id, path, state, reason, created_at) VALUES (?1, ?2, 'planned', ?3, ?4)",
                params![run_id, c.path.to_string_lossy(), c.reason, db::now_secs()],
            )?;
            let cid = tx.last_insert_rowid();
            for v in &c.variants {
                tx.execute(
                    "INSERT INTO conflict_variant (conflict_id, path, origin, origin_path, size, mtime_ns, healthy, is_suggested)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        cid,
                        v.path.to_string_lossy(),
                        v.origin,
                        v.origin_path.to_string_lossy(),
                        v.size,
                        v.mtime_ns,
                        v.healthy,
                        v.suggested
                    ],
                )?;
            }
            conflict_ids.push(cid);
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO import_item (run_id, seq, action, source_file_id, from_path, to_path, size, mtime_ns,
                                          content_hash, note, conflict_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for (seq, it) in plan.items.iter().enumerate() {
                let e = &entries[it.entry];
                stmt.execute(params![
                    run_id,
                    seq as i64,
                    it.action,
                    (!e.in_library).then_some(e.file_id),
                    e.abs.to_string_lossy(),
                    it.to.as_ref().map(|p| p.to_string_lossy().to_string()),
                    e.size,
                    e.mtime_ns,
                    e.full.map(|h| h.to_vec()),
                    it.note,
                    it.conflict.map(|c| conflict_ids[c]),
                ])?;
            }
            let base = plan.items.len();
            for (i, b) in bundles.iter().enumerate() {
                stmt.execute(params![
                    run_id,
                    (base + i) as i64,
                    "copy",
                    None::<i64>,
                    b.from.to_string_lossy(),
                    b.to.to_string_lossy().to_string(),
                    b.size,
                    b.mtime_ns,
                    None::<Vec<u8>>,
                    b.note,
                    None::<i64>,
                ])?;
            }
        }
        tx.commit()?;
        drop(conn);
        self.import_plan(run_id)
    }

    pub fn import_plan(&self, run_id: i64) -> Result<ImportPlan> {
        let conn = self.reader()?;
        conn.query_row(
            "SELECT r.id, r.state, s.root, r.dest_rel, r.files, r.to_copy, r.bytes_to_copy,
                    r.skipped_in_library, r.skipped_duplicate, r.conflicts, r.needs_review, r.batch_id
               FROM import_run r JOIN source s ON s.id = r.library_source_id WHERE r.id = ?1",
            [run_id],
            |r| {
                let root: String = r.get(2)?;
                let dest: String = r.get(3)?;
                Ok(ImportPlan {
                    run_id: r.get(0)?,
                    state: r.get(1)?,
                    dest_path: join_rel(Path::new(&root), &dest).to_string_lossy().to_string(),
                    files: r.get(4)?,
                    to_copy: r.get(5)?,
                    bytes_to_copy: r.get(6)?,
                    skipped_in_library: r.get(7)?,
                    skipped_duplicate: r.get(8)?,
                    conflicts: r.get(9)?,
                    needs_review: r.get(10)?,
                    batch_id: r.get(11)?,
                })
            },
        )
        .optional()?
        .with_context(|| format!("no import plan {run_id}"))
    }

    /// Items of a plan. `action` filters (e.g. "copy", "skip_in_library"); `conflicts_only`
    /// returns only items involved in a conflict.
    pub fn import_items(
        &self,
        run_id: i64,
        action: Option<&str>,
        conflicts_only: bool,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<ImportItem>> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare(
            "SELECT id, action, from_path, to_path, size, mtime_ns, note, conflict_id, state, error
               FROM import_item
              WHERE run_id = ?1 AND (?2 IS NULL OR action = ?2) AND (?3 = 0 OR conflict_id IS NOT NULL)
              ORDER BY conflict_id IS NULL, conflict_id, seq LIMIT ?4 OFFSET ?5",
        )?;
        let rows = stmt
            .query_map(
                params![run_id, action, conflicts_only, limit as i64, offset as i64],
                |r| {
                    Ok(ImportItem {
                        id: r.get(0)?,
                        action: r.get(1)?,
                        from_path: r.get(2)?,
                        to_path: r.get(3)?,
                        size: r.get(4)?,
                        mtime_ns: r.get(5)?,
                        note: r.get(6)?,
                        conflict_id: r.get(7)?,
                        state: r.get(8)?,
                        error: r.get(9)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Step 2: carry out a stored plan. Safe to call again after an interruption: finished items
    /// are skipped. Every change is journaled under one batch, so it can be undone as a whole.
    pub fn execute_import(
        &self,
        run_id: i64,
        progress: ImportProgressFn,
        cancel: &AtomicBool,
    ) -> Result<ImportResult> {
        let plan = self.import_plan(run_id)?;
        if plan.state == "undone" {
            bail!("import {run_id} was undone; plan it again");
        }
        let (lib_source_id, dest_rel): (i64, String) = self.reader()?.query_row(
            "SELECT library_source_id, dest_rel FROM import_run WHERE id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let lib_src = self.source(lib_source_id)?;
        let lib_root = PathBuf::from(&lib_src.root);
        if !lib_root.is_dir() {
            bail!("the library folder {} is not available", lib_root.display());
        }

        let batch_id = match plan.batch_id {
            Some(b) => b,
            None => {
                let conn = self.writer();
                conn.execute(
                    "INSERT INTO batch (kind, created_at, description) VALUES ('import', ?1, ?2)",
                    params![
                        db::now_secs(),
                        format!("Import {} files into {}", plan.to_copy, plan.dest_path)
                    ],
                )?;
                let b = conn.last_insert_rowid();
                conn.execute(
                    "UPDATE import_run SET batch_id = ?2, state = 'running' WHERE id = ?1",
                    params![run_id, b],
                )?;
                b
            }
        };

        let pending: Vec<(i64, String, String, Option<String>, i64, i64)> = {
            let conn = self.reader()?;
            let mut stmt = conn.prepare(
                "SELECT id, action, from_path, to_path, size, mtime_ns FROM import_item
                  WHERE run_id = ?1 AND state = 'planned' AND action IN ('rename_existing', 'copy')
                  ORDER BY seq",
            )?;
            let rows = stmt
                .query_map([run_id], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };

        let bytes_total: u64 = pending.iter().map(|p| p.4 as u64).sum();
        let mut res = ImportResult {
            run_id,
            batch_id,
            ..Default::default()
        };
        let total = pending.len() as u64;
        let mut bytes_done = 0u64;
        let mut copied_hashes: Vec<(PathBuf, [u8; 32])> = Vec::new();
        for (n, (item_id, action, from, to, size, mtime)) in pending.into_iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                res.cancelled = true;
                break;
            }
            progress(&ImportProgress {
                phase: "copying",
                done: n as u64,
                total,
                bytes_done,
                bytes_total,
            });
            let from = PathBuf::from(&from);
            let to = PathBuf::from(to.unwrap_or_default());
            let outcome = (|| -> Result<Option<[u8; 32]>> {
                check_unchanged(&from, size, mtime)?;
                match action.as_str() {
                    "rename_existing" => {
                        fsops::rename_no_replace(&from, &to)?;
                        self.journal(batch_id, "rename", Some(&from), Some(&to), size, None, None)?;
                        Ok(None)
                    }
                    _ => {
                        let created = create_parents(&to)?;
                        for dir in &created {
                            self.journal(batch_id, "mkdir", None, Some(dir), 0, None, None)?;
                        }
                        let h = fsops::copy_verified(&from, &to)?;
                        self.journal(
                            batch_id,
                            "copy",
                            Some(&from),
                            Some(&to),
                            size,
                            Some(&h),
                            None,
                        )?;
                        Ok(Some(h))
                    }
                }
            })();
            let conn = self.writer();
            match outcome {
                Ok(h) => {
                    conn.execute(
                        "UPDATE import_item SET state = 'done' WHERE id = ?1",
                        [item_id],
                    )?;
                    if let Some(h) = h {
                        res.copied += 1;
                        res.bytes += size as u64;
                        copied_hashes.push((to.clone(), h));
                    } else {
                        res.renamed += 1;
                    }
                }
                Err(e) => {
                    let msg = format!("{}: {e:#}", from.display());
                    conn.execute(
                        "UPDATE import_item SET state = 'failed', error = ?2 WHERE id = ?1",
                        params![item_id, msg],
                    )?;
                    res.failed += 1;
                    if res.errors.len() < 20 {
                        res.errors.push(msg);
                    }
                }
            }
            bytes_done += size as u64;
        }

        progress(&ImportProgress {
            phase: "finishing",
            done: total,
            total,
            bytes_done,
            bytes_total,
        });
        // Bring the index up to date for the destination, and remember the hashes we just proved.
        scan::scan_folder(
            self,
            lib_source_id,
            &dest_rel,
            &|_| {},
            &AtomicBool::new(false),
        )?;
        {
            let mut conn = self.writer();
            let tx = conn.transaction()?;
            for (path, h) in &copied_hashes {
                if let Ok(rel) = path.strip_prefix(&lib_root) {
                    let rel = crate::library::rel_string(rel);
                    let (dir, name) = match rel.rfind('/') {
                        Some(i) => (rel[..i].to_string(), rel[i + 1..].to_string()),
                        None => (String::new(), rel.clone()),
                    };
                    tx.execute(
                        "UPDATE file SET content_hash = ?1 WHERE id = (
                           SELECT f.id FROM file f JOIN folder d ON d.id = f.folder_id
                            WHERE f.source_id = ?2 AND d.rel_path = ?3 AND f.name = ?4)",
                        params![&h[..], lib_source_id, dir, name],
                    )?;
                }
            }
            let remaining: i64 = tx.query_row(
                "SELECT COUNT(*) FROM import_item WHERE run_id = ?1 AND state = 'planned' AND action IN ('rename_existing', 'copy')",
                [run_id],
                |r| r.get(0),
            )?;
            let state = if remaining > 0 || res.failed > 0 {
                "partial"
            } else {
                "done"
            };
            tx.execute(
                "UPDATE import_run SET state = ?2, finished_at = ?3 WHERE id = ?1",
                params![run_id, state, db::now_secs()],
            )?;
            tx.execute(
                "UPDATE batch SET state = ?2 WHERE id = ?1",
                params![batch_id, state],
            )?;
            // Conflicts become real once their files exist.
            tx.execute(
                "UPDATE conflict SET state = CASE reason WHEN 'clear_newest' THEN 'suggested' ELSE 'needs_review' END
                  WHERE run_id = ?1 AND state = 'planned'",
                [run_id],
            )?;
            tx.commit()?;
        }
        Ok(res)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn journal(
        &self,
        batch_id: i64,
        op: &str,
        from: Option<&Path>,
        to: Option<&Path>,
        size: i64,
        hash: Option<&[u8; 32]>,
        error: Option<&str>,
    ) -> Result<()> {
        self.writer().execute(
            "INSERT INTO journal (batch_id, created_at, op, from_path, to_path, size, content_hash, status, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                batch_id,
                db::now_secs(),
                op,
                from.map(|p| p.to_string_lossy().to_string()),
                to.map(|p| p.to_string_lossy().to_string()),
                size,
                hash.map(|h| h.to_vec()),
                if error.is_some() { "failed" } else { "done" },
                error
            ],
        )?;
        Ok(())
    }

    /// Recent batches (imports, later clean-ups and organise steps), newest first.
    pub fn history(&self, limit: usize) -> Result<Vec<BatchInfo>> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare(
            "SELECT b.id, b.kind, b.created_at, b.description, b.state,
                    (SELECT COUNT(*) FROM journal j WHERE j.batch_id = b.id AND j.status = 'done' AND j.op NOT IN ('mkdir', 'rmdir')),
                    (SELECT COUNT(*) FROM journal j WHERE j.batch_id = b.id AND j.status = 'undone' AND j.op NOT IN ('mkdir', 'rmdir'))
               FROM batch b ORDER BY b.id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit as i64], |r| {
                Ok(BatchInfo {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    created_at: r.get(2)?,
                    description: r.get(3)?,
                    state: r.get(4)?,
                    done: r.get(5)?,
                    undone: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Step 3: reverse a batch. Copies go to the OS trash (after checking they are still the
    /// file we made), renames are reversed, folders the batch created are removed when empty.
    pub fn undo_batch(&self, batch_id: i64) -> Result<UndoResult> {
        let state: Option<String> = self
            .reader()?
            .query_row("SELECT state FROM batch WHERE id = ?1", [batch_id], |r| {
                r.get(0)
            })
            .optional()?;
        match state.as_deref() {
            None => bail!("no batch {batch_id} (see history)"),
            Some("undone") => bail!("batch {batch_id} was already undone"),
            _ => {}
        }
        let entries: Vec<JournalRow> = {
            let conn = self.reader()?;
            let mut stmt = conn.prepare(
                "SELECT id, op, from_path, to_path, content_hash, mtime_ns FROM journal
                  WHERE batch_id = ?1 AND status = 'done' ORDER BY id DESC",
            )?;
            let rows = stmt
                .query_map([batch_id], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let mut res = UndoResult {
            batch_id,
            ..Default::default()
        };
        // Files put back outside the batch's scope: their folders are rescanned afterwards.
        let mut touched: Vec<PathBuf> = Vec::new();
        for (jid, op, from, to, h, mtime) in entries {
            if op == "trash" || op == "link" {
                if let Some(f) = &from {
                    touched.push(PathBuf::from(f));
                }
            }
            let outcome = (|| -> Result<()> {
                if op == "link" {
                    // from = the file that became a link, to = the copy it points at.
                    let path = PathBuf::from(from.as_deref().context("link without a path")?);
                    let keep = PathBuf::from(to.as_deref().context("link without a target")?);
                    return crate::dupes::unlink_copy(&path, &keep, mtime);
                }
                if op == "trash" {
                    // from = the original path, to = where the trash put it (when known).
                    let original = PathBuf::from(from.context("trash entry without a path")?);
                    if original.exists() {
                        bail!("{} exists again; not restored over it", original.display());
                    }
                    return trash::restore(&original, to.as_deref().map(Path::new));
                }
                let to = PathBuf::from(to.context("journal entry without a destination")?);
                match op.as_str() {
                    "copy" | "create" => {
                        if !to.exists() {
                            return Ok(()); // already gone
                        }
                        let now = hash::full_hash(&to)?;
                        if h.as_deref() != Some(&now[..]) {
                            bail!(
                                "{} changed since it was imported; left in place",
                                to.display()
                            );
                        }
                        trash::trash(&to)?;
                        Ok(())
                    }
                    "rename" => {
                        let from = PathBuf::from(from.context("rename without an origin")?);
                        let case_only = from.to_string_lossy().to_lowercase()
                            == to.to_string_lossy().to_lowercase();
                        if case_only {
                            std::fs::rename(&to, &from)?;
                        } else if to.is_dir() {
                            if from.exists() {
                                bail!("{} exists again; not moved back over it", from.display());
                            }
                            std::fs::rename(&to, &from)?;
                        } else {
                            fsops::move_file(&to, &from)?;
                        }
                        // Keep ids (and tags) by updating the index in place.
                        let _ = self.reindex_move(&to, &from);
                        Ok(())
                    }
                    "edit" => {
                        // from = backup of the original text, to = the edited note.
                        let backup = PathBuf::from(from.context("edit without a backup")?);
                        let now = std::fs::read(&to)?;
                        if h.as_deref() != Some(blake3::hash(&now).as_bytes().as_slice()) {
                            bail!(
                                "{} changed since it was updated; not restored",
                                to.display()
                            );
                        }
                        let original = std::fs::read(&backup)?;
                        let tmp = to.with_extension("docgait-restore-tmp");
                        std::fs::write(&tmp, original)?;
                        std::fs::rename(&tmp, &to)?;
                        Ok(())
                    }
                    "mkdir" => {
                        // Only if empty; anything put there since stays.
                        let _ = std::fs::remove_dir(&to);
                        Ok(())
                    }
                    "rmdir" => {
                        std::fs::create_dir_all(&to)?;
                        Ok(())
                    }
                    other => bail!("cannot undo {other}"),
                }
            })();
            let conn = self.writer();
            match outcome {
                Ok(()) => {
                    conn.execute("UPDATE journal SET status = 'undone' WHERE id = ?1", [jid])?;
                    if op != "mkdir" && op != "rmdir" {
                        res.undone += 1;
                    }
                }
                Err(e) => {
                    res.failed += 1;
                    if res.errors.len() < 20 {
                        res.errors.push(format!("{e:#}"));
                    }
                }
            }
        }
        // Mark the batch and its import, refresh the index for the destination.
        let run: Option<(i64, i64, String)> = self
            .reader()?
            .query_row(
                "SELECT id, library_source_id, dest_rel FROM import_run WHERE batch_id = ?1",
                [batch_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        {
            let state = if res.failed == 0 { "undone" } else { "partial" };
            let conn = self.writer();
            conn.execute(
                "UPDATE batch SET state = ?2 WHERE id = ?1",
                params![batch_id, state],
            )?;
            // Conflicts this batch resolved are open again.
            conn.execute(
                "UPDATE conflict SET state = CASE reason WHEN 'clear_newest' THEN 'suggested' ELSE 'needs_review' END,
                        resolved_at = NULL, resolution = NULL, resolved_batch_id = NULL
                  WHERE resolved_batch_id = ?1",
                [batch_id],
            )?;
            if let Some((run_id, _, _)) = &run {
                conn.execute(
                    "UPDATE import_run SET state = ?2 WHERE id = ?1",
                    params![run_id, state],
                )?;
                if res.failed == 0 {
                    conn.execute("DELETE FROM conflict WHERE run_id = ?1", [run_id])?;
                }
            }
        }
        let scoped = run.is_some()
            || self.reader()?.query_row(
                "SELECT scope_source_id IS NOT NULL FROM batch WHERE id = ?1",
                [batch_id],
                |r| r.get::<_, bool>(0),
            )?;
        if !scoped {
            let mut parents: Vec<PathBuf> = touched
                .iter()
                .filter_map(|p| p.parent().map(Path::to_path_buf))
                .collect();
            parents.sort();
            parents.dedup();
            // A folder already covered by a rescanned parent needs no scan of its own.
            let mut done: Vec<PathBuf> = Vec::new();
            for dir in parents {
                if done.iter().any(|d| dir.starts_with(d)) {
                    continue;
                }
                if let Some(src) = self.source_containing(&dir)? {
                    if !src.online {
                        continue;
                    }
                    let rel = dir
                        .strip_prefix(&src.root)
                        .map(crate::library::rel_string)
                        .unwrap_or_default();
                    scan::scan_folder(self, src.id, &rel, &|_| {}, &AtomicBool::new(false))?;
                    done.push(dir);
                }
            }
        }
        if let Some((_, src, dest)) = run {
            scan::scan_folder(self, src, &dest, &|_| {}, &AtomicBool::new(false))?;
        } else if let Some((src, rel)) = self
            .reader()?
            .query_row(
                "SELECT scope_source_id, scope_rel FROM batch WHERE id = ?1 AND scope_source_id IS NOT NULL",
                [batch_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
        {
            scan::scan_folder(self, src, &rel, &|_| {}, &AtomicBool::new(false))?;
        }
        Ok(res)
    }
}

// ---- folders that travel with their content ----

/// Excluded from indexing, but part of the folder that holds them: a vault without `.obsidian`
/// is no longer a vault, a repository without `.git` loses its history. They are copied as a
/// whole, and only when the destination does not have one yet (never merged).
const BUNDLES: &[(&str, &str)] = &[
    (".obsidian", "Obsidian vault settings"),
    (".git", "Git repository"),
    (".hg", "Mercurial repository"),
    (".svn", "Subversion working copy"),
    (".jj", "Jujutsu repository"),
];

pub(crate) struct BundleItem {
    from: PathBuf,
    to: PathBuf,
    size: i64,
    mtime_ns: i64,
    note: String,
}

/// Bundle folders next to (or above) copied files, mapped to where those files are going.
/// Never at the library root itself: that would turn the whole library into one vault or repo.
fn bundle_items(entries: &[Entry], plan: &Decided, lib_root: &Path) -> Vec<BundleItem> {
    // Source folder → destination folder, from the files that are copied.
    let mut dirs: HashMap<PathBuf, PathBuf> = HashMap::new();
    for it in &plan.items {
        let (Some(to), "copy") = (&it.to, it.action) else {
            continue;
        };
        let (Some(mut s), Some(mut d)) = (entries[it.entry].abs.parent(), to.parent()) else {
            continue;
        };
        // Ancestors map too, while their names match.
        loop {
            dirs.entry(s.to_path_buf())
                .or_insert_with(|| d.to_path_buf());
            match (s.parent(), d.parent()) {
                (Some(sp), Some(dp)) if s.file_name() == d.file_name() => {
                    s = sp;
                    d = dp;
                }
                _ => break,
            }
        }
    }
    let mut out = Vec::new();
    let mut taken: HashSet<PathBuf> = HashSet::new();
    let mut sorted: Vec<(PathBuf, PathBuf)> = dirs.into_iter().collect();
    sorted.sort();
    for (src_dir, dest_dir) in sorted {
        if dest_dir == lib_root || !dest_dir.starts_with(lib_root) {
            continue;
        }
        for (name, what) in BUNDLES {
            let from_dir = src_dir.join(name);
            let to_dir = dest_dir.join(name);
            if !from_dir.is_dir() || to_dir.exists() || !taken.insert(to_dir.clone()) {
                continue;
            }
            let note = format!("{what}, copied with its folder");
            let mut stack = vec![from_dir.clone()];
            while let Some(dir) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for e in rd.flatten() {
                    let p = e.path();
                    let Ok(md) = std::fs::symlink_metadata(&p) else {
                        continue;
                    };
                    if md.is_dir() {
                        stack.push(p);
                    } else if md.is_file() {
                        let rel = p.strip_prefix(&from_dir).unwrap_or(&p);
                        out.push(BundleItem {
                            to: to_dir.join(rel),
                            from: p,
                            size: md.len() as i64,
                            mtime_ns: crate::fsmeta::mtime_ns(&md),
                            note: note.clone(),
                        });
                    }
                }
            }
        }
    }
    out
}

// ---- deciding ----

struct PlannedItem {
    entry: usize,
    action: &'static str,
    to: Option<PathBuf>,
    note: Option<String>,
    conflict: Option<usize>,
}

struct PlannedConflict {
    path: PathBuf,
    reason: &'static str,
    needs_review: bool,
    variants: Vec<PlannedVariant>,
}

struct PlannedVariant {
    path: PathBuf,
    origin: String,
    origin_path: PathBuf,
    size: i64,
    mtime_ns: i64,
    healthy: bool,
    suggested: bool,
}

struct Decided {
    items: Vec<PlannedItem>,
    conflicts: Vec<PlannedConflict>,
    files: i64,
    to_copy: i64,
    bytes_to_copy: i64,
    skipped_in_library: i64,
    skipped_duplicate: i64,
}

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Key {
    Full([u8; 32]),
    Unique(usize),
}

pub(crate) fn key_of(entries: &[Entry], i: usize) -> Key {
    match entries[i].full {
        // Zero-byte files are all the same content.
        _ if entries[i].size == 0 => Key::Full([0; 32]),
        Some(h) => Key::Full(h),
        None => Key::Unique(i),
    }
}

/// Pure decision step: no I/O.
fn decide(
    entries: &[Entry],
    incoming: usize,
    lib_root: &Path,
    dest: &str,
    layout: Layout,
) -> Decided {
    let insensitive = fsops::case_insensitive_names();
    let path_key = |rel: &str| {
        if insensitive {
            rel.to_lowercase()
        } else {
            rel.to_string()
        }
    };

    let mut lib_by_key: HashMap<Key, usize> = HashMap::new();
    let mut lib_by_path: HashMap<String, usize> = HashMap::new();
    for i in incoming..entries.len() {
        lib_by_key.entry(key_of(entries, i)).or_insert(i);
        lib_by_path.insert(path_key(&entries[i].rel), i);
    }

    let mut out = Decided {
        items: Vec::new(),
        conflicts: Vec::new(),
        files: incoming as i64,
        to_copy: 0,
        bytes_to_copy: 0,
        skipped_in_library: 0,
        skipped_duplicate: 0,
    };

    // Group incoming files by content.
    let mut groups: HashMap<Key, Vec<usize>> = HashMap::new();
    for i in 0..incoming {
        groups.entry(key_of(entries, i)).or_default().push(i);
    }
    let mut keepers: Vec<usize> = Vec::new();
    let mut skips: Vec<PlannedItem> = Vec::new();
    let mut group_list: Vec<(Key, Vec<usize>)> = groups.into_iter().collect();
    group_list.sort_by_key(|(_, v)| v[0]);
    for (key, mut members) in group_list {
        if let Some(&li) = lib_by_key.get(&key) {
            for m in members {
                skips.push(PlannedItem {
                    entry: m,
                    action: "skip_in_library",
                    to: None,
                    note: Some(format!("already in the library at {}", entries[li].rel)),
                    conflict: None,
                });
                out.skipped_in_library += 1;
            }
            continue;
        }
        members.sort_by(|&a, &b| keeper_rank(&entries[a]).cmp(&keeper_rank(&entries[b])));
        let keeper = members[0];
        keepers.push(keeper);
        for &m in &members[1..] {
            skips.push(PlannedItem {
                entry: m,
                action: "skip_duplicate",
                to: None,
                note: Some(format!("same content as {}", entries[keeper].abs.display())),
                conflict: None,
            });
            out.skipped_duplicate += 1;
        }
    }

    // Targets.
    let target_rel = |e: &Entry| -> String {
        let mut parts: Vec<&str> = Vec::new();
        if !dest.is_empty() {
            parts.push(dest);
        }
        if layout == Layout::OwnFolder {
            parts.push(&e.sel_name);
        }
        parts.push(&e.rel);
        parts.join("/")
    };
    let mut by_target: HashMap<String, (String, Vec<usize>)> = HashMap::new();
    for &k in &keepers {
        let rel = target_rel(&entries[k]);
        by_target
            .entry(path_key(&rel))
            .or_insert_with(|| (rel, Vec::new()))
            .1
            .push(k);
    }
    // Names already taken in the library, or by planned items.
    let mut taken: HashSet<String> = lib_by_path.keys().cloned().collect();
    for k in by_target.keys() {
        taken.insert(k.clone());
    }

    let mut renames: Vec<PlannedItem> = Vec::new();
    let mut copies: Vec<PlannedItem> = Vec::new();
    let mut targets: Vec<(String, (String, Vec<usize>))> = by_target.into_iter().collect();
    targets.sort_by(|a, b| a.1 .0.cmp(&b.1 .0));
    for (pkey, (rel, incoming_here)) in targets {
        let existing = lib_by_path.get(&pkey).copied();
        if existing.is_none() && incoming_here.len() == 1 {
            let e = incoming_here[0];
            copies.push(PlannedItem {
                entry: e,
                action: "copy",
                to: Some(join_rel(lib_root, &rel)),
                note: None,
                conflict: None,
            });
            out.to_copy += 1;
            out.bytes_to_copy += entries[e].size;
            continue;
        }
        // A conflict: several different contents for one path.
        let mut variants: Vec<usize> = existing.into_iter().collect();
        variants.extend(&incoming_here);
        let mtimes: Vec<i64> = variants.iter().map(|&v| entries[v].mtime_ns).collect();
        let healthy: Vec<bool> = variants.iter().map(|&v| entries[v].size > 0).collect();
        let (winner, reason, needs_review) =
            choose_winner(&variants, &mtimes, &healthy, existing.is_some());
        let cidx = out.conflicts.len();
        let real_path = join_rel(lib_root, &rel);
        let mut planned = PlannedConflict {
            path: real_path.clone(),
            reason,
            needs_review,
            variants: Vec::new(),
        };
        for (vi, &v) in variants.iter().enumerate() {
            let e = &entries[v];
            let is_winner = vi == winner;
            let dest_path = if is_winner {
                real_path.clone()
            } else {
                let origin = if e.in_library {
                    "library"
                } else {
                    e.origin.as_str()
                };
                let name = unique_conflict_name(&rel, origin, e.mtime_ns, &mut taken, &path_key);
                join_rel(lib_root, &name)
            };
            planned.variants.push(PlannedVariant {
                path: dest_path.clone(),
                origin: if e.in_library {
                    "library".into()
                } else {
                    e.origin.clone()
                },
                origin_path: e.abs.clone(),
                size: e.size,
                mtime_ns: e.mtime_ns,
                healthy: healthy[vi],
                suggested: is_winner,
            });
            if e.in_library {
                if !is_winner {
                    renames.push(PlannedItem {
                        entry: v,
                        action: "rename_existing",
                        to: Some(dest_path),
                        note: Some("conflict: makes room for the newer version".into()),
                        conflict: Some(cidx),
                    });
                }
            } else {
                copies.push(PlannedItem {
                    entry: v,
                    action: "copy",
                    to: Some(dest_path),
                    note: Some(if is_winner {
                        "conflict: suggested version, keeps the name".into()
                    } else {
                        "conflict: kept next to the original".into()
                    }),
                    conflict: Some(cidx),
                });
                out.to_copy += 1;
                out.bytes_to_copy += e.size;
            }
        }
        out.conflicts.push(planned);
    }
    // Renames first (they make room), then copies, then the skips (for the record).
    out.items.extend(renames);
    out.items.extend(copies);
    out.items.extend(skips);
    out
}

/// Which variant keeps the real name. Healthy beats broken; then the newest, unless the dates
/// cannot be trusted, in which case the existing library file (or the first source) stays put
/// and the conflict needs review.
fn choose_winner(
    variants: &[usize],
    mtimes: &[i64],
    healthy: &[bool],
    has_existing: bool,
) -> (usize, &'static str, bool) {
    let healthy_idx: Vec<usize> = (0..variants.len()).filter(|&i| healthy[i]).collect();
    if healthy_idx.len() < variants.len() && healthy_idx.len() == 1 {
        return (healthy_idx[0], "broken_variant", false);
    }
    let pool: Vec<usize> = if healthy_idx.is_empty() {
        (0..variants.len()).collect()
    } else {
        healthy_idx
    };
    let pool_times: Vec<i64> = pool.iter().map(|&i| mtimes[i]).collect();
    if dates_unreliable(&pool_times) {
        // Placeholder: the existing library file (index 0 when present) or the first source.
        let _ = has_existing;
        return (pool[0], "dates_unreliable", true);
    }
    let newest = *pool.iter().max_by_key(|&&i| mtimes[i]).unwrap_or(&pool[0]);
    let reason = if pool.len() < variants.len() {
        "broken_variant"
    } else {
        "clear_newest"
    };
    (newest, reason, false)
}

/// Modified dates that should not be used to pick a winner.
pub(crate) fn dates_unreliable(mtimes_ns: &[i64]) -> bool {
    const SEC: i64 = 1_000_000_000;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        * SEC;
    let y1990 = 631_152_000 * SEC;
    if mtimes_ns
        .iter()
        .any(|&t| t > now + 86_400 * SEC || t < y1990)
    {
        return true;
    }
    for (i, &a) in mtimes_ns.iter().enumerate() {
        for &b in &mtimes_ns[i + 1..] {
            let d = (a - b).abs();
            // FAT/exFAT keeps 2-second times; copies can land a moment apart.
            if d < 3 * SEC {
                return true;
            }
            // An exact number of hours apart: a time-zone or daylight-saving shift.
            if d <= 26 * 3600 * SEC {
                let r = (d / SEC) % 3600;
                if r <= 2 || r >= 3598 {
                    return true;
                }
            }
        }
    }
    false
}

/// Lower is a better keeper: clean names, then short paths, then older files.
fn keeper_rank(e: &Entry) -> (bool, usize, i64) {
    let name = e.rel.rsplit('/').next().unwrap_or("").to_lowercase();
    let unclean = name.contains(" copy")
        || name.starts_with("copy of")
        || name.contains("-copy")
        || name.contains("_copy")
        || (1..=9).any(|n| name.contains(&format!(" ({n})")) || name.contains(&format!("({n})")));
    (unclean, e.abs.as_os_str().len(), e.mtime_ns)
}

/// `Taxes/W2.pdf` → `Taxes/W2.conflict-CopyB-2021-03-04.pdf`, made unique against `taken`.
fn unique_conflict_name(
    rel: &str,
    origin: &str,
    mtime_ns: i64,
    taken: &mut HashSet<String>,
    path_key: &dyn Fn(&str) -> String,
) -> String {
    let (dir, name) = match rel.rfind('/') {
        Some(i) => (&rel[..=i], &rel[i + 1..]),
        None => ("", rel),
    };
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let origin: String = origin
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(40)
        .collect();
    let origin = if origin.is_empty() {
        "source".to_string()
    } else {
        origin
    };
    let date = ymd(mtime_ns);
    for n in 1.. {
        let suffix = if n == 1 {
            String::new()
        } else {
            format!("-{n}")
        };
        let candidate = format!("{dir}{stem}.conflict-{origin}-{date}{suffix}{ext}");
        if taken.insert(path_key(&candidate)) {
            return candidate;
        }
    }
    unreachable!()
}

/// YYYY-MM-DD (UTC) for nanoseconds since the epoch.
pub(crate) fn ymd(ns: i64) -> String {
    let days = ns.div_euclid(86_400 * 1_000_000_000);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

pub(crate) fn normalize_rel(rel: &str) -> String {
    rel.split(['/', '\\'])
        .filter(|p| !p.is_empty() && *p != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// `folder_rel` relative to `base` ('' when they are the same).
fn strip_rel(folder_rel: &str, base: &str) -> String {
    if base.is_empty() {
        folder_rel.to_string()
    } else if folder_rel == base {
        String::new()
    } else {
        folder_rel
            .strip_prefix(&format!("{base}/"))
            .unwrap_or(folder_rel)
            .to_string()
    }
}

fn check_unchanged(path: &Path, size: i64, mtime_ns: i64) -> Result<()> {
    let md = std::fs::metadata(path).with_context(|| format!("{} is gone", path.display()))?;
    if md.len() as i64 != size || crate::fsmeta::mtime_ns(&md) != mtime_ns {
        bail!("changed since the plan was made; plan the import again");
    }
    Ok(())
}

/// Creates the missing folders above `path`, returning the ones it created (top first).
fn create_parents(path: &Path) -> Result<Vec<PathBuf>> {
    let mut missing = Vec::new();
    let mut p = path.parent();
    while let Some(dir) = p {
        if dir.exists() {
            break;
        }
        missing.push(dir.to_path_buf());
        p = dir.parent();
    }
    missing.reverse();
    for d in &missing {
        std::fs::create_dir(d).with_context(|| format!("cannot create {}", d.display()))?;
    }
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_names() {
        let mut taken = HashSet::new();
        let key = |s: &str| s.to_lowercase();
        let ns = 1_614_816_000 * 1_000_000_000; // 2021-03-04
        assert_eq!(
            unique_conflict_name("Taxes/W2.pdf", "Copy B", ns, &mut taken, &key),
            "Taxes/W2.conflict-Copy-B-2021-03-04.pdf"
        );
        assert_eq!(
            unique_conflict_name("Taxes/W2.pdf", "Copy B", ns, &mut taken, &key),
            "Taxes/W2.conflict-Copy-B-2021-03-04-2.pdf"
        );
        assert_eq!(
            unique_conflict_name("README", "x", ns, &mut taken, &key),
            "README.conflict-x-2021-03-04"
        );
        assert_eq!(
            unique_conflict_name(".env", "x", ns, &mut taken, &key),
            ".env.conflict-x-2021-03-04"
        );
    }

    #[test]
    fn unreliable_dates() {
        const S: i64 = 1_000_000_000;
        let base = 1_700_000_000 * S;
        assert!(dates_unreliable(&[base, base + S])); // seconds apart
        assert!(dates_unreliable(&[base, base + 3600 * S])); // exactly an hour
        assert!(dates_unreliable(&[base, base + 7 * 3600 * S + S])); // time-zone shift, ~1 s off
        assert!(dates_unreliable(&[base, 100 * S])); // 1970
        assert!(!dates_unreliable(&[base, base + 86_400 * 3 * S])); // days apart
        assert!(!dates_unreliable(&[base, base + 1234 * S]));
    }

    #[test]
    fn dates_format() {
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(951_782_400 * 1_000_000_000), "2000-02-29");
    }
}
