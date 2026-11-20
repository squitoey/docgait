//! The `walk` stage: enumerate a source (names level: path, size, dates, kind) and bring the
//! index in line with what is on disk.
//!
//! A parallel walker feeds a bounded channel; this thread writes batches in short transactions,
//! so readers (the UI) are never blocked for long and memory stays flat on huge trees.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use crossbeam_channel::{bounded, RecvTimeoutError};
use ignore::WalkState;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::library::rel_string;
use crate::{db, exclude, fsmeta, kind, Library};

const BATCH: usize = 2000;
const FLUSH_EVERY: Duration = Duration::from_millis(250);
const PROGRESS_EVERY: Duration = Duration::from_millis(100);
const MAX_ERRORS_KEPT: usize = 20;

#[derive(Serialize, Clone, Debug)]
pub struct ScanProgress {
    pub source_id: i64,
    /// "walking" or "finishing"
    pub phase: &'static str,
    pub files: u64,
    pub folders: u64,
    pub errors: u64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct ScanSummary {
    pub source_id: i64,
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    /// Files no longer on disk: dropped from the index, or kept as missing on removable sources.
    pub gone: u64,
    pub errors: u64,
    /// The first few error messages (permission denied, unreadable folders, ...).
    pub error_samples: Vec<String>,
    pub cancelled: bool,
    pub millis: u64,
}

pub type ProgressFn<'a> = &'a (dyn Fn(&ScanProgress) + Sync);

enum Msg {
    Dir {
        rel: String,
    },
    File {
        rel_dir: String,
        name: String,
        size: u64,
        mtime_ns: i64,
        created_ns: Option<i64>,
        dev: i64,
        ino: i64,
        package: bool,
    },
    Error(String),
}

/// Scans one source. Safe to run while the UI reads the index.
pub fn scan_source(
    lib: &Library,
    source_id: i64,
    progress: ProgressFn,
    cancel: &AtomicBool,
) -> Result<ScanSummary> {
    scan_scope(lib, source_id, None, progress, cancel)
}

/// Rescans one folder of a source (and everything below it), e.g. after the file watcher saw a
/// change there. `rel` is relative to the source root, '/'-separated; "" means the whole source.
pub fn scan_folder(
    lib: &Library,
    source_id: i64,
    rel: &str,
    progress: ProgressFn,
    cancel: &AtomicBool,
) -> Result<ScanSummary> {
    let rel = rel.trim_matches('/');
    scan_scope(
        lib,
        source_id,
        (!rel.is_empty()).then_some(rel),
        progress,
        cancel,
    )
}

fn scan_scope(
    lib: &Library,
    source_id: i64,
    scope: Option<&str>,
    progress: ProgressFn,
    cancel: &AtomicBool,
) -> Result<ScanSummary> {
    let started = Instant::now();
    let source = lib.source(source_id)?;
    let root = PathBuf::from(&source.root);
    if !root.is_dir() {
        bail!("{} is not available (offline or removed)", root.display());
    }
    let scope_path = scope.map(|rel| crate::library::join_rel(&root, rel));
    let walker = exclude::walker(&root, &source.walk_options(), scope_path.as_deref())?;

    let gen: i64 = {
        let conn = lib.writer();
        conn.execute(
            "UPDATE source SET scan_gen = scan_gen + 1 WHERE id = ?1",
            [source_id],
        )?;
        conn.query_row(
            "SELECT scan_gen FROM source WHERE id = ?1",
            [source_id],
            |r| r.get(0),
        )?
    };

    let mut folders = load_folders(&lib.reader()?, source_id)?;
    let mut sum = ScanSummary {
        source_id,
        ..Default::default()
    };

    let (tx, rx) = bounded::<Msg>(10_000);
    std::thread::scope(|scope| -> Result<()> {
        let root_w = root.clone();
        scope.spawn(move || {
            walker.build_parallel().run(|| {
                let tx = tx.clone();
                let root = root_w.clone();
                Box::new(move |res| {
                    if cancel.load(Ordering::Relaxed) {
                        return WalkState::Quit;
                    }
                    let (msg, state) = entry_msg(&root, res);
                    if let Some(m) = msg {
                        if tx.send(m).is_err() {
                            return WalkState::Quit;
                        }
                    }
                    state
                })
            });
        });

        let mut batch = Vec::with_capacity(BATCH);
        let mut last_flush = Instant::now();
        let mut last_progress = Instant::now();
        loop {
            let done = match rx.recv_timeout(FLUSH_EVERY) {
                Ok(m) => {
                    batch.push(m);
                    while batch.len() < BATCH {
                        match rx.try_recv() {
                            Ok(m) => batch.push(m),
                            Err(_) => break,
                        }
                    }
                    false
                }
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => true,
            };
            if batch.len() >= BATCH
                || (!batch.is_empty() && (done || last_flush.elapsed() >= FLUSH_EVERY))
            {
                write_batch(lib, source_id, gen, &mut folders, &mut batch, &mut sum)?;
                last_flush = Instant::now();
            }
            if last_progress.elapsed() >= PROGRESS_EVERY {
                last_progress = Instant::now();
                progress(&progress_of(&sum, "walking"));
            }
            if done {
                break;
            }
        }
        Ok(())
    })?;

    sum.cancelled = cancel.load(Ordering::Relaxed);
    if !sum.cancelled {
        progress(&progress_of(&sum, "finishing"));
        sum.gone = finish(lib, source_id, gen, source.removable, scope)?;
    }
    sum.millis = started.elapsed().as_millis() as u64;
    progress(&progress_of(&sum, "finishing"));
    Ok(sum)
}

fn progress_of(sum: &ScanSummary, phase: &'static str) -> ScanProgress {
    ScanProgress {
        source_id: sum.source_id,
        phase,
        files: sum.files,
        folders: sum.folders,
        errors: sum.errors,
    }
}

/// Turns one walker result into a message, and tells the walker whether to descend.
fn entry_msg(
    root: &Path,
    res: std::result::Result<ignore::DirEntry, ignore::Error>,
) -> (Option<Msg>, WalkState) {
    let entry = match res {
        Ok(e) => e,
        Err(e) => return (Some(Msg::Error(e.to_string())), WalkState::Continue),
    };
    if entry.depth() == 0 {
        return (None, WalkState::Continue);
    }
    let Some(ft) = entry.file_type() else {
        return (None, WalkState::Continue);
    };
    // Symlinks are not followed or indexed; their targets are indexed if they are in a source.
    if ft.is_symlink() {
        return (None, WalkState::Continue);
    }
    let path = entry.path();
    let Ok(rel) = path.strip_prefix(root) else {
        return (None, WalkState::Continue);
    };
    if ft.is_dir() && !exclude::is_package(path) {
        return (
            Some(Msg::Dir {
                rel: rel_string(rel),
            }),
            WalkState::Continue,
        );
    }
    let md = match entry.metadata() {
        Ok(m) => m,
        Err(e) => {
            return (
                Some(Msg::Error(format!("{}: {e}", path.display()))),
                WalkState::Continue,
            )
        }
    };
    let (dev, ino) = fsmeta::ident(path, &md);
    let created_ns = md
        .created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64);
    let rel_dir = rel.parent().map(rel_string).unwrap_or_default();
    let name = entry.file_name().to_string_lossy().to_string();
    let package = ft.is_dir();
    let msg = Msg::File {
        rel_dir,
        name,
        size: md.len(),
        mtime_ns: fsmeta::mtime_ns(&md),
        created_ns,
        dev,
        ino,
        package,
    };
    // A package is one item: never descend into it.
    (
        Some(msg),
        if package {
            WalkState::Skip
        } else {
            WalkState::Continue
        },
    )
}

fn load_folders(conn: &Connection, source_id: i64) -> Result<HashMap<String, i64>> {
    let mut stmt = conn.prepare("SELECT rel_path, id FROM folder WHERE source_id = ?1")?;
    let map = stmt
        .query_map([source_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(map)
}

fn write_batch(
    lib: &Library,
    source_id: i64,
    gen: i64,
    folders: &mut HashMap<String, i64>,
    batch: &mut Vec<Msg>,
    sum: &mut ScanSummary,
) -> Result<()> {
    let mut conn = lib.writer();
    let tx = conn.transaction()?;
    {
        let mut file_stmt = tx.prepare_cached(
            "INSERT INTO file (source_id, folder_id, name, ext, kind, size, mtime_ns, created_ns, dev, ino, state, seen_gen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'present', ?11)
             ON CONFLICT (folder_id, name) DO UPDATE SET
               ext = excluded.ext, kind = excluded.kind, created_ns = excluded.created_ns,
               dev = excluded.dev, ino = excluded.ino, state = 'present', seen_gen = excluded.seen_gen,
               partial_hash = CASE WHEN file.size = excluded.size AND file.mtime_ns = excluded.mtime_ns
                                   THEN file.partial_hash END,
               content_hash = CASE WHEN file.size = excluded.size AND file.mtime_ns = excluded.mtime_ns
                                   THEN file.content_hash END,
               size = excluded.size, mtime_ns = excluded.mtime_ns",
        )?;
        for msg in batch.drain(..) {
            match msg {
                Msg::Dir { rel } => {
                    ensure_folder(&tx, source_id, gen, folders, &rel)?;
                    sum.folders += 1;
                }
                Msg::File {
                    rel_dir,
                    name,
                    size,
                    mtime_ns,
                    created_ns,
                    dev,
                    ino,
                    package,
                } => {
                    let folder_id = ensure_folder(&tx, source_id, gen, folders, &rel_dir)?;
                    let ext = kind::extension(&name);
                    let k = if package {
                        "package"
                    } else {
                        kind::kind_of(&ext)
                    };
                    file_stmt.execute(params![
                        source_id,
                        folder_id,
                        name,
                        ext,
                        k,
                        size as i64,
                        mtime_ns,
                        created_ns,
                        dev,
                        ino,
                        gen
                    ])?;
                    sum.files += 1;
                    sum.bytes += size;
                }
                Msg::Error(e) => {
                    sum.errors += 1;
                    if sum.error_samples.len() < MAX_ERRORS_KEPT {
                        sum.error_samples.push(e);
                    }
                }
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// Id of the folder at `rel`, creating it (and any missing parents) and marking it seen.
fn ensure_folder(
    conn: &Connection,
    source_id: i64,
    gen: i64,
    folders: &mut HashMap<String, i64>,
    rel: &str,
) -> Result<i64> {
    if let Some(&id) = folders.get(rel) {
        conn.prepare_cached(
            "UPDATE folder SET seen_gen = ?2, state = 'present' WHERE id = ?1 AND seen_gen <> ?2",
        )?
        .execute(params![id, gen])?;
        return Ok(id);
    }
    let (parent_rel, name) = match rel.rfind('/') {
        Some(i) => (&rel[..i], &rel[i + 1..]),
        None if rel.is_empty() => ("", ""),
        None => ("", rel),
    };
    let parent_id = if rel.is_empty() {
        None
    } else {
        Some(ensure_folder(conn, source_id, gen, folders, parent_rel)?)
    };
    let id: i64 = conn
        .prepare_cached(
            "INSERT INTO folder (source_id, parent_id, name, rel_path, seen_gen) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (source_id, rel_path) DO UPDATE SET seen_gen = excluded.seen_gen, state = 'present'
             RETURNING id",
        )?
        .query_row(params![source_id, parent_id, name, rel, gen], |r| r.get(0))?;
    folders.insert(rel.to_string(), id);
    Ok(id)
}

/// After a complete walk: drop (or, on removable sources, flag) what was not seen, and refresh
/// folder counts. With `scope`, only that folder and below are considered. Returns the number of
/// files gone.
fn finish(
    lib: &Library,
    source_id: i64,
    gen: i64,
    removable: bool,
    scope: Option<&str>,
) -> Result<u64> {
    // Folders in scope: the scope folder itself and everything below it.
    let (in_scope, scope_arg) = match scope {
        None => ("1", String::new()),
        Some(rel) => (
            "(rel_path = ?3 OR substr(rel_path, 1, length(?3) + 1) = ?3 || '/')",
            rel.to_string(),
        ),
    };
    let mut conn = lib.writer();
    let tx = conn.transaction()?;
    if scope.is_none() {
        tx.execute(
            "UPDATE folder SET seen_gen = ?2, state = 'present' WHERE source_id = ?1 AND rel_path = ''",
            params![source_id, gen],
        )?;
    }
    let scoped_files =
        format!("folder_id IN (SELECT id FROM folder WHERE source_id = ?1 AND {in_scope})");
    let run = |sql: &str| -> rusqlite::Result<usize> {
        if scope.is_some() {
            tx.execute(sql, params![source_id, gen, scope_arg])
        } else {
            tx.execute(sql, params![source_id, gen])
        }
    };
    let gone = if removable {
        run(&format!(
            "UPDATE folder SET state = 'missing' WHERE source_id = ?1 AND seen_gen < ?2 AND {in_scope}"
        ))?;
        run(&format!(
            "UPDATE file SET state = 'missing'
              WHERE source_id = ?1 AND seen_gen < ?2 AND state <> 'missing' AND {scoped_files}"
        ))?
    } else {
        let n = run(&format!(
            "DELETE FROM file WHERE source_id = ?1 AND seen_gen < ?2 AND {scoped_files}"
        ))?;
        run(&format!(
            "DELETE FROM folder WHERE source_id = ?1 AND seen_gen < ?2 AND {in_scope}"
        ))?;
        n
    };
    // Refresh counts for the folders in scope and the ones above it.
    let mut targets = format!("source_id = ?1 AND {in_scope}");
    let ancestors: Vec<String> = scope
        .map(|rel| {
            let parts: Vec<&str> = rel.split('/').collect();
            (0..parts.len()).map(|n| parts[..n].join("/")).collect()
        })
        .unwrap_or_default();
    if !ancestors.is_empty() {
        let list = ancestors
            .iter()
            .map(|a| format!("'{}'", a.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");
        targets = format!("source_id = ?1 AND ({in_scope} OR rel_path IN ({list}))");
    }
    let counts = format!(
        "UPDATE folder SET
           file_count   = (SELECT COUNT(*) FROM file f WHERE f.folder_id = folder.id AND f.state = 'present'),
           total_size   = (SELECT COALESCE(SUM(size), 0) FROM file f WHERE f.folder_id = folder.id AND f.state = 'present'),
           folder_count = (SELECT COUNT(*) FROM folder c WHERE c.parent_id = folder.id AND c.state = 'present')
         WHERE {targets} AND ?2 = ?2"
    );
    run(&counts)?;
    if scope.is_none() {
        tx.execute(
            "UPDATE source SET last_scan_at = ?2 WHERE id = ?1",
            params![source_id, db::now_secs()],
        )?;
    }
    tx.commit()?;
    Ok(gone as u64)
}

/// Scans every online source in turn.
pub fn scan_all(
    lib: &Library,
    progress: ProgressFn,
    cancel: &AtomicBool,
) -> Result<Vec<ScanSummary>> {
    let mut out = Vec::new();
    for s in lib.sources()? {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if !s.online {
            continue;
        }
        out.push(scan_source(lib, s.id, progress, cancel)?);
    }
    Ok(out)
}

/// Whether a scan has ever completed for this source.
pub fn has_scanned(lib: &Library, source_id: i64) -> Result<bool> {
    Ok(lib
        .reader()?
        .query_row(
            "SELECT last_scan_at FROM source WHERE id = ?1",
            [source_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten()
        .is_some())
}
