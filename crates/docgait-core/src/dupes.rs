//! Exact duplicates across the library and every source (photo-bomb's exact groups, generalised).
//!
//! Finding hashes only files that share a size (size → partial → full BLAKE3), so a names-only
//! source costs nothing until two files could be copies. Groups are derived on demand from the
//! stored hashes, never stored. Acting on them follows photo-bomb's rules, enforced here:
//!  - nothing is permanently deleted (trash), or the bytes stay reachable (hard link);
//!  - a plan that would remove every copy of some content is blocked;
//!  - every file and the copy that is kept are re-hashed right before each action;
//!  - every action is journaled and can be undone.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::import::{ImportProgress, ImportProgressFn};
use crate::library::{join_rel, Role};
use crate::{db, fsmeta, hash, trash, Library};

#[derive(Serialize, Clone, Debug, Default)]
pub struct DupeScan {
    /// Files compared (everything in scope; most never need hashing).
    pub files: u64,
    pub groups: u64,
    pub duplicate_files: u64,
    pub reclaimable: i64,
    pub cancelled: bool,
}

/// Which groups to show. A group is shown when any member matches.
#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct DupeFilter {
    pub source_id: Option<i64>,
    pub folder_id: Option<i64>,
    pub kind: Option<String>,
    /// Only groups with copies on import sources that the library already has.
    #[serde(default)]
    pub safe_only: bool,
    /// Include groups marked "not duplicates".
    #[serde(default)]
    pub include_ignored: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct DupeMember {
    pub file_id: i64,
    pub path: String,
    pub source_id: i64,
    pub role: Role,
    pub size: i64,
    pub mtime_ns: i64,
    /// The suggested copy to keep.
    pub keeper: bool,
    /// Shares its bytes on disk with another member already.
    pub hardlinked: bool,
    /// On an import source while the library has the same content: safe to clean up.
    pub safe: bool,
    /// Its source is not connected; shown but never touched.
    pub offline: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct DupeGroup {
    /// Content hash (hex); identifies the group.
    pub hash: String,
    pub size: i64,
    pub kind: String,
    pub keeper: i64,
    pub members: Vec<DupeMember>,
    /// Bytes freed by keeping one copy (hard links already share bytes).
    pub reclaimable: i64,
    pub in_library: bool,
    pub ignored: bool,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct DupePage {
    pub total_groups: usize,
    pub total_files: usize,
    pub total_reclaimable: i64,
    /// Copies on import sources that the library already has.
    pub safe_files: usize,
    pub safe_bytes: i64,
    pub groups: Vec<DupeGroup>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DedupeOp {
    /// Move to the system trash.
    Trash,
    /// Replace with a hard link to the kept copy (same drive only). The path stays; the bytes
    /// are stored once.
    Hardlink,
}

#[derive(Serialize, Clone, Debug)]
pub struct DedupeItem {
    pub file_id: i64,
    pub path: String,
    pub size: i64,
    /// "ok" | "skip" | "blocked"
    pub status: &'static str,
    pub note: Option<String>,
    /// The copy that is kept (and linked to, for hard links).
    pub keep: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct DedupePlan {
    pub op: DedupeOp,
    pub items: Vec<DedupeItem>,
    pub ok: usize,
    pub skipped: usize,
    pub blocked: usize,
    /// Bytes freed.
    pub bytes: i64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct DedupeResult {
    pub batch_id: i64,
    pub done: u64,
    pub skipped: u64,
    pub bytes: i64,
    pub errors: Vec<String>,
    pub cancelled: bool,
}

/// One present, non-empty file that has a content hash shared with another file.
#[derive(Clone, Debug)]
struct Row {
    id: i64,
    source_id: i64,
    path: PathBuf,
    kind: String,
    size: i64,
    mtime_ns: i64,
    ident: (i64, i64),
    hash: Vec<u8>,
}

struct Ctx {
    roles: HashMap<i64, Role>,
    online: HashMap<i64, bool>,
}

impl Ctx {
    fn role(&self, r: &Row) -> Role {
        self.roles
            .get(&r.source_id)
            .copied()
            .unwrap_or(Role::Import)
    }
    fn online(&self, r: &Row) -> bool {
        self.online.get(&r.source_id).copied().unwrap_or(false)
    }
}

impl Library {
    /// Hashes whatever is needed to find exact duplicates. `folder_id` limits the search to
    /// files that could be copies of something in that folder; `None` covers every online source.
    pub fn find_duplicates(
        &self,
        folder_id: Option<i64>,
        progress: ImportProgressFn,
        cancel: &AtomicBool,
    ) -> Result<DupeScan> {
        let scope = match folder_id {
            Some(id) => {
                let f = self.folder(id)?;
                Some((f.source_id, f.rel_path))
            }
            None => None,
        };
        let mut entries = Vec::new();
        for src in self.sources()?.into_iter().filter(|s| s.online) {
            match &scope {
                // Files in scope are "incoming"; everything else only gets hashed when it
                // collides with one of them.
                Some((sid, rel)) if *sid == src.id => {
                    let inside = self.files_under(&src, rel, false)?;
                    let ids: HashSet<i64> = inside.iter().map(|e| e.file_id).collect();
                    entries.extend(inside);
                    entries.extend(
                        self.files_under(&src, "", true)?
                            .into_iter()
                            .filter(|e| !ids.contains(&e.file_id)),
                    );
                }
                Some(_) => entries.extend(self.files_under(&src, "", true)?),
                None => entries.extend(self.files_under(&src, "", false)?),
            }
        }
        // Empty files are all "the same"; they are not duplicates worth showing.
        entries.retain(|e| e.size > 0);
        let files = entries.iter().filter(|e| !e.in_library).count() as u64;
        self.hash_colliding(&mut entries, progress, cancel)?;
        let page = self.duplicate_groups(
            &DupeFilter {
                folder_id,
                ..Default::default()
            },
            0,
            0,
        )?;
        Ok(DupeScan {
            files,
            groups: page.total_groups as u64,
            duplicate_files: page.total_files.saturating_sub(page.total_groups) as u64,
            reclaimable: page.total_reclaimable,
            cancelled: cancel.load(Ordering::Relaxed),
        })
    }

    fn ctx(&self) -> Result<Ctx> {
        let sources = self.sources()?;
        Ok(Ctx {
            roles: sources.iter().map(|s| (s.id, s.role)).collect(),
            online: sources.iter().map(|s| (s.id, s.online)).collect(),
        })
    }

    /// Every present file whose content hash is shared, grouped by hash.
    fn hashed_groups(&self, only_hash: Option<&[u8]>) -> Result<HashMap<Vec<u8>, Vec<Row>>> {
        let conn = self.reader()?;
        let roots: HashMap<i64, PathBuf> = self
            .sources()?
            .into_iter()
            .map(|s| (s.id, PathBuf::from(s.root)))
            .collect();
        let sql = "SELECT f.id, f.source_id, d.rel_path, f.name, f.kind, f.size, f.mtime_ns, f.dev, f.ino, f.content_hash
                     FROM file f JOIN folder d ON d.id = f.folder_id
                    WHERE f.state = 'present' AND f.size > 0 AND f.content_hash IS NOT NULL
                      AND (?1 IS NULL OR f.content_hash = ?1)
                      AND f.content_hash IN (
                        SELECT content_hash FROM file
                         WHERE state = 'present' AND size > 0 AND content_hash IS NOT NULL
                           AND (?1 IS NULL OR content_hash = ?1)
                         GROUP BY content_hash HAVING COUNT(*) > 1)";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map(params![only_hash], |r| {
                let source_id: i64 = r.get(1)?;
                let rel: String = r.get(2)?;
                let name: String = r.get(3)?;
                Ok((
                    source_id,
                    rel,
                    name,
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    (r.get(7)?, r.get(8)?),
                    r.get::<_, Vec<u8>>(9)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out: HashMap<Vec<u8>, Vec<Row>> = HashMap::new();
        for (source_id, rel, name, id, kind, size, mtime_ns, ident, h) in rows {
            let Some(root) = roots.get(&source_id) else {
                continue;
            };
            out.entry(h.clone()).or_default().push(Row {
                id,
                source_id,
                path: join_rel(root, &rel).join(name),
                kind,
                size,
                mtime_ns,
                ident,
                hash: h,
            });
        }
        Ok(out)
    }

    fn ignored_hashes(&self) -> Result<HashSet<Vec<u8>>> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare("SELECT content_hash FROM dup_ignore")?;
        let set = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(set)
    }

    /// Groups of byte-identical files, biggest saving first. `limit` 0 = all.
    pub fn duplicate_groups(
        &self,
        filter: &DupeFilter,
        offset: usize,
        limit: usize,
    ) -> Result<DupePage> {
        let ctx = self.ctx()?;
        let ignored = self.ignored_hashes()?;
        let scope = match filter.folder_id {
            Some(id) => {
                let f = self.folder(id)?;
                let src = self.source(f.source_id)?;
                Some(join_rel(Path::new(&src.root), &f.rel_path))
            }
            None => None,
        };
        let matches = |r: &Row| {
            filter.source_id.is_none_or(|s| s == r.source_id)
                && scope.as_ref().is_none_or(|p| r.path.starts_with(p))
                && filter.kind.as_ref().is_none_or(|k| k == &r.kind)
        };
        let mut groups: Vec<DupeGroup> = self
            .hashed_groups(None)?
            .into_iter()
            .filter(|(h, rows)| {
                (filter.include_ignored || !ignored.contains(h)) && rows.iter().any(&matches)
            })
            .map(|(h, rows)| build_group(&ctx, rows, ignored.contains(&h)))
            .filter(|g| !filter.safe_only || g.members.iter().any(|m| m.safe))
            .collect();
        groups.sort_by(|a, b| {
            b.reclaimable
                .cmp(&a.reclaimable)
                .then_with(|| a.hash.cmp(&b.hash))
        });
        let safe: Vec<&DupeMember> = groups
            .iter()
            .flat_map(|g| g.members.iter().filter(|m| m.safe))
            .collect();
        let mut page = DupePage {
            total_groups: groups.len(),
            total_files: groups.iter().map(|g| g.members.len()).sum(),
            total_reclaimable: groups.iter().map(|g| g.reclaimable).sum(),
            safe_files: safe.len(),
            safe_bytes: safe.iter().map(|m| m.size).sum(),
            groups: Vec::new(),
        };
        let take = if limit == 0 { usize::MAX } else { limit };
        page.groups = groups.into_iter().skip(offset).take(take).collect();
        Ok(page)
    }

    /// The files to act on for "select all but the keeper" (or only the safe copies).
    /// Offline files, and groups whose keeper is offline, are left alone.
    pub fn auto_select_duplicates(&self, filter: &DupeFilter, safe_only: bool) -> Result<Vec<i64>> {
        let page = self.duplicate_groups(filter, 0, 0)?;
        Ok(page
            .groups
            .iter()
            .filter(|g| g.members.iter().any(|m| m.keeper && !m.offline))
            .flat_map(|g| {
                g.members
                    .iter()
                    .filter(|m| !m.keeper && !m.offline && (!safe_only || m.safe))
                    .map(|m| m.file_id)
            })
            .collect())
    }

    /// Number of other present copies of each file (0 when unique or not hashed yet).
    pub fn duplicate_counts(&self, ids: &[i64]) -> Result<HashMap<i64, i64>> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare_cached(
            "SELECT (SELECT COUNT(*) FROM file g
                      WHERE g.content_hash = f.content_hash AND g.id <> f.id AND g.state = 'present')
               FROM file f WHERE f.id = ?1 AND f.content_hash IS NOT NULL AND f.size > 0",
        )?;
        let mut out = HashMap::new();
        for id in ids {
            if let Ok(n) = stmt.query_row([id], |r| r.get::<_, i64>(0)) {
                if n > 0 {
                    out.insert(*id, n);
                }
            }
        }
        Ok(out)
    }

    /// The group a file belongs to, if it has copies.
    pub fn duplicate_group_of(&self, file_id: i64) -> Result<Option<DupeGroup>> {
        let h: Option<Vec<u8>> = self.reader()?.query_row(
            "SELECT content_hash FROM file WHERE id = ?1",
            [file_id],
            |r| r.get(0),
        )?;
        let Some(h) = h else { return Ok(None) };
        let ctx = self.ctx()?;
        let ignored = self.ignored_hashes()?;
        Ok(self
            .hashed_groups(Some(&h))?
            .remove(&h)
            .map(|rows| build_group(&ctx, rows, ignored.contains(&h))))
    }

    /// "These copies are intentional": the group is hidden from the Duplicates view.
    pub fn ignore_duplicate_group(&self, hash_hex: &str, ignore: bool) -> Result<()> {
        let h = from_hex(hash_hex)?;
        let conn = self.writer();
        if ignore {
            conn.execute(
                "INSERT OR IGNORE INTO dup_ignore (content_hash, created_at) VALUES (?1, ?2)",
                params![h, db::now_secs()],
            )?;
        } else {
            conn.execute("DELETE FROM dup_ignore WHERE content_hash = ?1", [h])?;
        }
        Ok(())
    }

    /// What `dedupe` would do with these files, without touching anything.
    pub fn plan_dedupe(&self, ids: &[i64], op: DedupeOp) -> Result<DedupePlan> {
        let ctx = self.ctx()?;
        let selected: HashSet<i64> = ids.iter().copied().collect();
        let mut by_hash: HashMap<Vec<u8>, Vec<Row>> = HashMap::new();
        let mut items = Vec::new();
        let mut cache: HashMap<Vec<u8>, Vec<Row>> = HashMap::new();
        for &id in ids {
            let h: Option<(Option<Vec<u8>>, String, i64)> = {
                let conn = self.reader()?;
                conn.query_row(
                    "SELECT content_hash, state, size FROM file WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .ok()
            };
            let path = self
                .file_path(id)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let skip = |note: &str, size: i64| DedupeItem {
                file_id: id,
                path: path.clone(),
                size,
                status: "skip",
                note: Some(note.to_string()),
                keep: None,
            };
            match h {
                None => items.push(skip("not in the index", 0)),
                Some((_, state, size)) if state != "present" => {
                    items.push(skip("missing; rescan", size))
                }
                Some((None, _, size)) => {
                    items.push(skip("not hashed; find duplicates first", size))
                }
                Some((Some(h), _, _)) => {
                    if !cache.contains_key(&h) {
                        let rows = self.hashed_groups(Some(&h))?.remove(&h).unwrap_or_default();
                        cache.insert(h.clone(), rows);
                    }
                    by_hash.entry(h).or_default();
                }
            }
        }
        for (h, _) in by_hash {
            let rows = cache.remove(&h).unwrap_or_default();
            let mut rows_sorted = rows.clone();
            rows_sorted.sort_by(|a, b| keeper_cmp(&ctx, a, b));
            // What stays: unselected copies that are on disk right now.
            let kept: Vec<&Row> = rows_sorted
                .iter()
                .filter(|r| !selected.contains(&r.id) && ctx.online(r))
                .collect();
            for r in rows_sorted.iter().filter(|r| selected.contains(&r.id)) {
                let mut item = DedupeItem {
                    file_id: r.id,
                    path: r.path.to_string_lossy().to_string(),
                    size: r.size,
                    status: "ok",
                    note: None,
                    keep: None,
                };
                if kept.is_empty() {
                    item.status = "blocked";
                    item.note = Some("every copy is selected; one must stay".into());
                } else if !ctx.online(r) {
                    item.status = "skip";
                    item.note = Some("its source is offline".into());
                } else if crate::exclude::ignore_file_in_use(&r.path) {
                    item.status = "skip";
                    item.note = Some(
                        "an ignore file that still decides what is indexed in its folder".into(),
                    );
                } else if let Err(e) = self.check_writable(&r.path) {
                    item.status = "skip";
                    item.note = Some(format!("{e:#}"));
                } else {
                    match op {
                        DedupeOp::Trash => {
                            item.keep = Some(kept[0].path.to_string_lossy().to_string());
                        }
                        DedupeOp::Hardlink => {
                            if kept.iter().any(|k| k.ident == r.ident) {
                                item.status = "skip";
                                item.note = Some("already a hard link to a kept copy".into());
                            } else if let Some(k) = kept.iter().find(|k| k.ident.0 == r.ident.0) {
                                item.keep = Some(k.path.to_string_lossy().to_string());
                            } else {
                                item.status = "skip";
                                item.note =
                                    Some("no kept copy on the same drive to link to".into());
                            }
                        }
                    }
                }
                items.push(item);
            }
        }
        items.sort_by(|a, b| a.path.cmp(&b.path));
        let count = |s: &str| items.iter().filter(|i| i.status == s).count();
        Ok(DedupePlan {
            op,
            ok: count("ok"),
            skipped: count("skip"),
            blocked: count("blocked"),
            bytes: items
                .iter()
                .filter(|i| i.status == "ok")
                .map(|i| i.size)
                .sum(),
            items,
        })
    }

    /// Trashes (or hard-links) the selected duplicates. Blocked and skipped files are left
    /// alone; everything else is re-verified right before it is touched. One undoable batch.
    pub fn dedupe(
        &self,
        ids: &[i64],
        op: DedupeOp,
        progress: ImportProgressFn,
        cancel: &AtomicBool,
    ) -> Result<DedupeResult> {
        let plan = self.plan_dedupe(ids, op)?;
        if plan.blocked > 0 {
            bail!(
                "{} file(s) are the last copy of their content; deselect one copy in each group",
                plan.blocked
            );
        }
        let verb = match op {
            DedupeOp::Trash => "Trash",
            DedupeOp::Hardlink => "Hard-link",
        };
        let batch_id = {
            let conn = self.writer();
            conn.execute(
                "INSERT INTO batch (kind, created_at, description) VALUES ('dedupe', ?1, ?2)",
                params![
                    db::now_secs(),
                    format!("{verb} {} duplicate file(s)", plan.ok)
                ],
            )?;
            conn.last_insert_rowid()
        };
        let mut res = DedupeResult {
            batch_id,
            skipped: plan.skipped as u64,
            ..Default::default()
        };
        let ok: Vec<&DedupeItem> = plan.items.iter().filter(|i| i.status == "ok").collect();
        let total = ok.len() as u64;
        let bytes_total: u64 = ok.iter().map(|i| i.size as u64).sum();
        let mut bytes_done = 0u64;
        // Kept copies are verified once per batch.
        let mut verified_keep: HashSet<String> = HashSet::new();
        for (n, item) in ok.into_iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                res.cancelled = true;
                break;
            }
            progress(&ImportProgress {
                phase: "deduplicating",
                done: n as u64,
                total,
                bytes_done,
                bytes_total,
            });
            bytes_done += item.size as u64;
            let outcome = self.dedupe_one(batch_id, item, op, &mut verified_keep);
            match outcome {
                Ok(()) => {
                    res.done += 1;
                    res.bytes += item.size;
                }
                Err(e) => {
                    res.skipped += 1;
                    if res.errors.len() < 20 {
                        res.errors.push(format!("{}: {e:#}", item.path));
                    }
                }
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
        Ok(res)
    }

    fn dedupe_one(
        &self,
        batch_id: i64,
        item: &DedupeItem,
        op: DedupeOp,
        verified_keep: &mut HashSet<String>,
    ) -> Result<()> {
        let path = PathBuf::from(&item.path);
        let keep = PathBuf::from(item.keep.as_deref().context("no copy to keep")?);
        let (want, size, mtime_ns): (Vec<u8>, i64, i64) = self.reader()?.query_row(
            "SELECT content_hash, size, mtime_ns FROM file WHERE id = ?1 AND state = 'present'",
            [item.file_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let md = std::fs::metadata(&path).context("gone")?;
        if md.len() as i64 != size || fsmeta::mtime_ns(&md) != mtime_ns {
            bail!("changed since it was indexed; rescan");
        }
        if hash::full_hash(&path)?[..] != want[..] {
            bail!("no longer matches its copies");
        }
        if !verified_keep.contains(&item.keep.clone().unwrap_or_default()) {
            if !keep.is_file() || hash::full_hash(&keep)?[..] != want[..] {
                bail!("the kept copy {} changed or is gone", keep.display());
            }
            verified_keep.insert(keep.to_string_lossy().to_string());
        }
        let hash_arr = <[u8; 32]>::try_from(want.as_slice()).ok();
        match op {
            DedupeOp::Trash => {
                let at = trash::trash(&path)?;
                self.journal_dedupe(
                    batch_id,
                    "trash",
                    &path,
                    at.as_deref(),
                    size,
                    hash_arr,
                    None,
                )?;
                self.forget_file(item.file_id)?;
            }
            DedupeOp::Hardlink => {
                let tmp = temp_sibling(&path);
                let _ = std::fs::remove_file(&tmp);
                std::fs::hard_link(&keep, &tmp).context("cannot create the hard link")?;
                if let Err(e) = std::fs::rename(&tmp, &path) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e).context("cannot replace the file with the link");
                }
                self.journal_dedupe(
                    batch_id,
                    "link",
                    &path,
                    Some(&keep),
                    size,
                    hash_arr,
                    Some(mtime_ns),
                )?;
                let md = std::fs::metadata(&path)?;
                let (dev, ino) = fsmeta::ident(&path, &md);
                self.writer().execute(
                    "UPDATE file SET dev = ?2, ino = ?3, mtime_ns = ?4 WHERE id = ?1",
                    params![item.file_id, dev, ino, fsmeta::mtime_ns(&md)],
                )?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn journal_dedupe(
        &self,
        batch_id: i64,
        op: &str,
        from: &Path,
        to: Option<&Path>,
        size: i64,
        hash: Option<[u8; 32]>,
        mtime_ns: Option<i64>,
    ) -> Result<()> {
        self.writer().execute(
            "INSERT INTO journal (batch_id, created_at, op, from_path, to_path, size, content_hash, status, mtime_ns)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'done', ?8)",
            params![
                batch_id,
                db::now_secs(),
                op,
                from.to_string_lossy().to_string(),
                to.map(|p| p.to_string_lossy().to_string()),
                size,
                hash.map(|h| h.to_vec()),
                mtime_ns
            ],
        )?;
        Ok(())
    }

    /// Drops a trashed file from the index (undo rescans its folder to bring it back).
    fn forget_file(&self, id: i64) -> Result<()> {
        let conn = self.writer();
        let folder: i64 =
            conn.query_row("SELECT folder_id FROM file WHERE id = ?1", [id], |r| {
                r.get(0)
            })?;
        conn.execute("DELETE FROM file WHERE id = ?1", [id])?;
        conn.execute(
            "UPDATE folder SET
               file_count = (SELECT COUNT(*) FROM file f WHERE f.folder_id = folder.id AND f.state = 'present'),
               total_size = (SELECT COALESCE(SUM(size), 0) FROM file f WHERE f.folder_id = folder.id AND f.state = 'present')
             WHERE id = ?1",
            [folder],
        )?;
        Ok(())
    }
}

/// Undo of a hard link: gives `path` its own copy of the bytes again, with its old date.
pub(crate) fn unlink_copy(path: &Path, keep: &Path, mtime_ns: Option<i64>) -> Result<()> {
    let (Some(a), Some(b)) = (fsmeta::ident_of(path), fsmeta::ident_of(keep)) else {
        bail!("{} or {} is gone", path.display(), keep.display());
    };
    if a != b {
        return Ok(()); // already separate
    }
    let tmp = temp_sibling(path);
    std::fs::copy(keep, &tmp)?;
    if let Some(ns) = mtime_ns {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(ns.max(0) as u64);
        std::fs::File::options()
            .write(true)
            .open(&tmp)?
            .set_modified(t)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn temp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.docgait-tmp-{}", std::process::id()))
}

fn from_hex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        bail!("bad hash {s:?}");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).context("bad hash"))
        .collect()
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Lower is cleaner: penalises "copy", "(2)", " 2" style names.
fn name_noise(path: &Path) -> u32 {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut n = 0;
    if stem.contains("copy") || stem.contains("duplicate") || stem.contains(".conflict-") {
        n += 2;
    }
    if stem.ends_with(')') && stem.contains('(') {
        n += 2;
    }
    if stem.split(' ').count() > 1
        && stem
            .rsplit(' ')
            .next()
            .is_some_and(|t| t.parse::<u32>().is_ok())
    {
        n += 1;
    }
    n
}

fn role_rank(r: Role) -> u8 {
    match r {
        Role::Library => 0,
        Role::Indexed => 1,
        Role::Import => 2,
    }
}

/// Keeper order: on disk now, in the library, cleanest name, shortest path, oldest.
fn keeper_cmp(ctx: &Ctx, a: &Row, b: &Row) -> CmpOrdering {
    (!ctx.online(a))
        .cmp(&!ctx.online(b))
        .then(role_rank(ctx.role(a)).cmp(&role_rank(ctx.role(b))))
        .then(name_noise(&a.path).cmp(&name_noise(&b.path)))
        .then(a.path.as_os_str().len().cmp(&b.path.as_os_str().len()))
        .then(a.mtime_ns.cmp(&b.mtime_ns))
        .then(a.id.cmp(&b.id))
}

fn build_group(ctx: &Ctx, mut rows: Vec<Row>, ignored: bool) -> DupeGroup {
    rows.sort_by(|a, b| keeper_cmp(ctx, a, b));
    let keeper = rows[0].id;
    let in_library = rows
        .iter()
        .any(|r| ctx.role(r) == Role::Library && ctx.online(r));
    let mut ident_count: HashMap<(i64, i64), usize> = HashMap::new();
    for r in &rows {
        *ident_count.entry(r.ident).or_default() += 1;
    }
    let live: HashSet<(i64, i64)> = rows
        .iter()
        .filter(|r| ctx.online(r))
        .map(|r| r.ident)
        .collect();
    let size = rows[0].size;
    DupeGroup {
        hash: to_hex(&rows[0].hash),
        size,
        kind: rows[0].kind.clone(),
        keeper,
        reclaimable: size * (live.len() as i64 - 1).max(0),
        in_library,
        ignored,
        members: rows
            .into_iter()
            .map(|r| DupeMember {
                file_id: r.id,
                path: r.path.to_string_lossy().to_string(),
                source_id: r.source_id,
                role: ctx.role(&r),
                size: r.size,
                mtime_ns: r.mtime_ns,
                keeper: r.id == keeper,
                hardlinked: ident_count[&r.ident] > 1,
                safe: in_library && ctx.role(&r) == Role::Import && ctx.online(&r),
                offline: !ctx.online(&r),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise() {
        assert_eq!(name_noise(Path::new("/a/W2.pdf")), 0);
        assert!(name_noise(Path::new("/a/W2 copy.pdf")) > 0);
        assert!(name_noise(Path::new("/a/W2 (2).pdf")) > 0);
        assert!(name_noise(Path::new("/a/W2 2.pdf")) > 0);
    }

    #[test]
    fn hex_round_trip() {
        let b = vec![0u8, 1, 0xab, 0xff];
        assert_eq!(from_hex(&to_hex(&b)).unwrap(), b);
    }
}
