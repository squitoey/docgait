//! Working through conflicts, IDE style. Conflict variants sit next to the real file as
//! `name.conflict-<source>-<date>.ext`; this module lists them, resolves them (keep one, keep
//! all under new names, merge text, or just close), and prepares merges.
//!
//! Every resolution is one journaled batch: removed variants go to the trash, so undo brings
//! them back and reopens the conflict.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{bail, Context, Result};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::library::{join_rel, rel_string};
use crate::{db, scan, trash, Library};

/// Largest file offered in the merge editor.
const MAX_TEXT: u64 = 5 * 1024 * 1024;

#[derive(Serialize, Clone, Debug)]
pub struct ConflictVariant {
    pub path: String,
    pub origin: String,
    pub origin_path: String,
    pub size: i64,
    pub mtime_ns: i64,
    pub healthy: bool,
    /// The suggested version (it holds the real name after the import).
    pub is_suggested: bool,
    /// This variant is the file under the real name right now.
    pub is_current: bool,
    pub exists: bool,
    /// Small enough and plain text: can be merged.
    pub text: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct ConflictView {
    pub id: i64,
    /// The real name (absolute).
    pub path: String,
    pub name: String,
    /// Folder relative to the library root.
    pub folder: String,
    pub state: String,
    pub reason: String,
    pub created_at: i64,
    pub variants: Vec<ConflictVariant>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Resolution {
    /// The file under the real name stays; every other variant goes to the trash.
    KeepCurrent,
    /// This variant takes the real name; the others go to the trash.
    KeepVariant { path: String },
    /// Every variant stays; the conflict files get these new names (same folder).
    KeepAll { names: Vec<(String, String)> },
    /// The merged text is saved under the real name; the variants go to the trash.
    Merge { text: String },
    /// Leave the files as they are; just close the conflict.
    MarkResolved,
}

impl Resolution {
    fn label(&self) -> String {
        match self {
            Resolution::KeepCurrent => "keep current".into(),
            Resolution::KeepVariant { path } => format!(
                "keep {}",
                Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            ),
            Resolution::KeepAll { .. } => "keep all".into(),
            Resolution::Merge { .. } => "merged".into(),
            Resolution::MarkResolved => "marked resolved".into(),
        }
    }
}

/// What the merge editor starts from.
#[derive(Serialize, Clone, Debug)]
pub struct MergeStart {
    pub left_label: String,
    pub right_label: String,
    pub left: String,
    pub right: String,
    /// The common base used for a three-way merge, when there was one.
    pub base_label: Option<String>,
    /// The starting result: merged where possible, with `<<<<<<<` / `=======` / `>>>>>>>` blocks
    /// where the two sides disagree.
    pub result: String,
    /// No disagreements left in `result`.
    pub clean: bool,
}

impl Library {
    /// Open conflicts (and resolved ones too, with `include_resolved`), oldest first.
    pub fn conflicts(&self, include_resolved: bool) -> Result<Vec<ConflictView>> {
        self.discover_conflicts()?;
        let lib_root = self
            .library_source()?
            .map(|s| PathBuf::from(s.root))
            .unwrap_or_default();
        let conn = self.reader()?;
        let mut stmt = conn.prepare(
            "SELECT id, path, state, reason, created_at FROM conflict
              WHERE state IN ('needs_review', 'suggested') OR (?1 AND state = 'resolved')
              ORDER BY state = 'resolved', path",
        )?;
        let rows = stmt
            .query_map([include_resolved], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut vstmt = conn.prepare(
            "SELECT path, origin, origin_path, size, mtime_ns, healthy, is_suggested
               FROM conflict_variant WHERE conflict_id = ?1 ORDER BY is_suggested DESC, mtime_ns DESC",
        )?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, path, state, reason, created_at) in rows {
            let real = PathBuf::from(&path);
            let variants = vstmt
                .query_map([id], |r| {
                    let p: String = r.get(0)?;
                    let pb = PathBuf::from(&p);
                    let exists = pb.is_file();
                    let size: i64 = r.get(3)?;
                    Ok(ConflictVariant {
                        is_current: pb == real,
                        text: exists && is_text(&pb),
                        exists,
                        path: p,
                        origin: r.get(1)?,
                        origin_path: r.get(2)?,
                        size,
                        mtime_ns: r.get(4)?,
                        healthy: r.get(5)?,
                        is_suggested: r.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let folder = real
                .parent()
                .and_then(|p| p.strip_prefix(&lib_root).ok())
                .map(rel_string)
                .unwrap_or_default();
            out.push(ConflictView {
                id,
                name: real
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                folder,
                path,
                state,
                reason,
                created_at,
                variants,
            });
        }
        Ok(out)
    }

    pub fn open_conflict_count(&self) -> Result<i64> {
        Ok(self.reader()?.query_row(
            "SELECT COUNT(*) FROM conflict WHERE state IN ('needs_review', 'suggested')",
            [],
            |r| r.get(0),
        )?)
    }

    /// Registers `.conflict-` files in the library that no conflict knows about (e.g. after the
    /// index was rebuilt, or files copied in by hand). They open as "needs review".
    pub fn discover_conflicts(&self) -> Result<usize> {
        let Some(lib_src) = self.library_source()? else {
            return Ok(0);
        };
        let root = PathBuf::from(&lib_src.root);
        let conn = self.reader()?;
        let known: HashSet<String> = conn
            .prepare(
                "SELECT v.path FROM conflict_variant v JOIN conflict c ON c.id = v.conflict_id",
            )?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut stmt = conn.prepare(
            "SELECT d.rel_path, f.name, f.size, f.mtime_ns FROM file f JOIN folder d ON d.id = f.folder_id
              WHERE f.source_id = ?1 AND f.state = 'present' AND f.name LIKE '%.conflict-%'",
        )?;
        let found = stmt
            .query_map([lib_src.id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        drop(conn);
        // real path → variants (path, origin, size, mtime)
        let mut groups: BTreeMap<PathBuf, Vec<(PathBuf, String, i64, i64)>> = BTreeMap::new();
        for (dir, name, size, mtime) in found {
            let Some((real_name, origin)) = parse_conflict_name(&name) else {
                continue;
            };
            let path = join_rel(&root, &dir).join(&name);
            if known.contains(&*path.to_string_lossy()) {
                continue;
            }
            let real = join_rel(&root, &dir).join(real_name);
            groups
                .entry(real)
                .or_default()
                .push((path, origin, size, mtime));
        }
        let n = groups.len();
        if n == 0 {
            return Ok(0);
        }
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        for (real, variants) in groups {
            tx.execute(
                "INSERT INTO conflict (path, state, reason, created_at) VALUES (?1, 'needs_review', 'found_on_disk', ?2)",
                params![real.to_string_lossy(), db::now_secs()],
            )?;
            let cid = tx.last_insert_rowid();
            if let Ok(md) = std::fs::metadata(&real) {
                tx.execute(
                    "INSERT OR IGNORE INTO conflict_variant (conflict_id, path, origin, origin_path, size, mtime_ns, healthy, is_suggested)
                     VALUES (?1, ?2, 'library', ?2, ?3, ?4, ?5, 1)",
                    params![
                        cid,
                        real.to_string_lossy(),
                        md.len() as i64,
                        crate::fsmeta::mtime_ns(&md),
                        md.len() > 0
                    ],
                )?;
            }
            for (p, origin, size, mtime) in variants {
                tx.execute(
                    "INSERT OR IGNORE INTO conflict_variant (conflict_id, path, origin, origin_path, size, mtime_ns, healthy, is_suggested)
                     VALUES (?1, ?2, ?3, ?2, ?4, ?5, ?6, 0)",
                    params![cid, p.to_string_lossy(), origin, size, mtime, size > 0],
                )?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Resolves one conflict. Returns the batch id (for undo).
    pub fn resolve_conflict(&self, id: i64, resolution: &Resolution) -> Result<i64> {
        let c = self
            .conflicts(false)?
            .into_iter()
            .find(|c| c.id == id)
            .with_context(|| format!("no open conflict {id}"))?;
        let batch_id = self.new_conflict_batch(
            &format!("Resolve conflict: {} ({})", c.name, resolution.label()),
            &c,
        )?;
        self.apply_resolution(batch_id, &c, resolution)?;
        self.finish_conflict_batch(batch_id, std::slice::from_ref(&c.folder))?;
        Ok(batch_id)
    }

    /// Resolves every conflict with a clear suggestion by keeping the suggested (current) file.
    /// One batch, one undo. Returns (batch id, how many were resolved).
    pub fn accept_suggestions(&self) -> Result<(i64, usize)> {
        let open: Vec<ConflictView> = self
            .conflicts(false)?
            .into_iter()
            .filter(|c| {
                c.state == "suggested" && c.variants.iter().any(|v| v.is_current && v.exists)
            })
            .collect();
        if open.is_empty() {
            bail!("no conflicts with a clear suggestion");
        }
        let (src, _) = self.library_scope(&open[0])?;
        let batch_id = {
            let conn = self.writer();
            conn.execute(
                "INSERT INTO batch (kind, created_at, description, scope_source_id, scope_rel)
                 VALUES ('conflict', ?1, ?2, ?3, '')",
                params![
                    db::now_secs(),
                    format!("Accept {} suggested conflict resolutions", open.len()),
                    src
                ],
            )?;
            conn.last_insert_rowid()
        };
        let mut folders = Vec::new();
        for c in &open {
            self.apply_resolution(batch_id, c, &Resolution::KeepCurrent)?;
            folders.push(c.folder.clone());
        }
        self.finish_conflict_batch(batch_id, &folders)?;
        Ok((batch_id, open.len()))
    }

    fn library_scope(&self, c: &ConflictView) -> Result<(i64, String)> {
        let src = self.library_source()?.context("no library folder")?;
        Ok((src.id, c.folder.clone()))
    }

    fn new_conflict_batch(&self, description: &str, c: &ConflictView) -> Result<i64> {
        let (src, rel) = self.library_scope(c)?;
        let conn = self.writer();
        conn.execute(
            "INSERT INTO batch (kind, created_at, description, scope_source_id, scope_rel)
             VALUES ('conflict', ?1, ?2, ?3, ?4)",
            params![db::now_secs(), description, src, rel],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn finish_conflict_batch(&self, batch_id: i64, folders: &[String]) -> Result<()> {
        self.writer()
            .execute("UPDATE batch SET state = 'done' WHERE id = ?1", [batch_id])?;
        if let Some(src) = self.library_source()? {
            let mut seen = HashSet::new();
            for f in folders {
                if seen.insert(f.clone()) {
                    scan::scan_folder(self, src.id, f, &|_| {}, &AtomicBool::new(false))?;
                }
            }
        }
        Ok(())
    }

    fn apply_resolution(
        &self,
        batch_id: i64,
        c: &ConflictView,
        resolution: &Resolution,
    ) -> Result<()> {
        let real = PathBuf::from(&c.path);
        let others = |keep: &Path| -> Vec<PathBuf> {
            c.variants
                .iter()
                .map(|v| PathBuf::from(&v.path))
                .filter(|p| p != keep && p.is_file())
                .collect()
        };
        match resolution {
            Resolution::KeepCurrent => {
                if !real.is_file() {
                    bail!(
                        "{} is missing; keep another version instead",
                        real.display()
                    );
                }
                for p in others(&real) {
                    self.trash_logged(batch_id, &p)?;
                }
            }
            Resolution::KeepVariant { path } => {
                let chosen = PathBuf::from(path);
                if !c.variants.iter().any(|v| v.path == *path) {
                    bail!("{path} is not part of this conflict");
                }
                if !chosen.is_file() {
                    bail!("{} is missing", chosen.display());
                }
                if chosen != real {
                    if real.is_file() {
                        self.trash_logged(batch_id, &real)?;
                    }
                    crate::fsops::rename_no_replace(&chosen, &real)?;
                    self.journal(
                        batch_id,
                        "rename",
                        Some(&chosen),
                        Some(&real),
                        0,
                        None,
                        None,
                    )?;
                }
                for p in others(&real) {
                    if p != chosen {
                        self.trash_logged(batch_id, &p)?;
                    }
                }
            }
            Resolution::KeepAll { names } => {
                for (from, new_name) in names {
                    let from = PathBuf::from(from);
                    if !c.variants.iter().any(|v| Path::new(&v.path) == from) {
                        bail!("{} is not part of this conflict", from.display());
                    }
                    if new_name.is_empty()
                        || new_name.contains(['/', '\\'])
                        || new_name == "."
                        || new_name == ".."
                    {
                        bail!("{new_name:?} is not a valid file name");
                    }
                    let to = from.with_file_name(new_name);
                    if to == from {
                        continue;
                    }
                    crate::fsops::rename_no_replace(&from, &to)?;
                    self.journal(batch_id, "rename", Some(&from), Some(&to), 0, None, None)?;
                }
            }
            Resolution::Merge { text } => {
                if text.contains("<<<<<<<") && text.contains(">>>>>>>") {
                    bail!("the merge still has unresolved blocks (<<<<<<< … >>>>>>>)");
                }
                if real.is_file() {
                    self.trash_logged(batch_id, &real)?;
                }
                let h = write_new(&real, text.as_bytes())?;
                self.journal(
                    batch_id,
                    "create",
                    None,
                    Some(&real),
                    text.len() as i64,
                    Some(&h),
                    None,
                )?;
                for p in others(&real) {
                    self.trash_logged(batch_id, &p)?;
                }
            }
            Resolution::MarkResolved => {}
        }
        self.writer().execute(
            "UPDATE conflict SET state = 'resolved', resolved_at = ?2, resolution = ?3, resolved_batch_id = ?4
              WHERE id = ?1",
            params![c.id, db::now_secs(), resolution.label(), batch_id],
        )?;
        Ok(())
    }

    fn trash_logged(&self, batch_id: i64, path: &Path) -> Result<()> {
        let size = std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0);
        let at = trash::trash(path)?;
        self.journal(
            batch_id,
            "trash",
            Some(path),
            at.as_deref(),
            size,
            None,
            None,
        )
    }

    /// Sets up the merge editor for the current file (left) and `variant` (right).
    pub fn merge_start(&self, id: i64, variant: &str) -> Result<MergeStart> {
        let c = self
            .conflicts(false)?
            .into_iter()
            .find(|c| c.id == id)
            .with_context(|| format!("no open conflict {id}"))?;
        let current = c
            .variants
            .iter()
            .find(|v| v.is_current && v.exists)
            .context("the file under the real name is missing")?;
        let other = c
            .variants
            .iter()
            .find(|v| v.path == variant && v.exists)
            .context("that version is missing")?;
        let left = read_text(Path::new(&current.path))?;
        let right = read_text(Path::new(&other.path))?;
        // With three or more versions, the oldest of the rest is the common base.
        let base = c
            .variants
            .iter()
            .filter(|v| v.exists && v.path != current.path && v.path != other.path && v.text)
            .min_by_key(|v| v.mtime_ns);
        let left_label = format!("current ({})", current.origin);
        let right_label = other.origin.clone();
        let (result, clean, base_label) = match base {
            Some(b) => {
                let base_text = read_text(Path::new(&b.path))?;
                match diffy::merge(&base_text, &left, &right) {
                    Ok(merged) => (merged, true, Some(b.origin.clone())),
                    Err(marked) => (
                        relabel(&marked, &left_label, &right_label),
                        false,
                        Some(b.origin.clone()),
                    ),
                }
            }
            None => {
                let (text, clean) = two_way(&left, &right, &left_label, &right_label);
                (text, clean, None)
            }
        };
        Ok(MergeStart {
            left_label,
            right_label,
            left,
            right,
            base_label,
            result,
            clean,
        })
    }
}

/// Writes a new file (temp name, then rename; never replaces). Returns its hash.
fn write_new(path: &Path, bytes: &[u8]) -> Result<[u8; 32]> {
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let tmp = path.with_file_name(format!(
        ".{}.docgait-tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    if let Err(e) = crate::fsops::rename_no_replace(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(*blake3::hash(bytes).as_bytes())
}

pub fn is_text(path: &Path) -> bool {
    let Ok(md) = std::fs::metadata(path) else {
        return false;
    };
    if md.len() > MAX_TEXT {
        return false;
    }
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    !bytes.contains(&0) && std::str::from_utf8(&bytes).is_ok()
}

fn read_text(path: &Path) -> Result<String> {
    if !is_text(path) {
        bail!(
            "{} is not a plain text file (or is over 5 MB)",
            path.display()
        );
    }
    Ok(std::fs::read_to_string(path)?)
}

/// Two-way merge: lines both sides share are kept, each disagreement becomes a marked block.
pub fn two_way(left: &str, right: &str, left_label: &str, right_label: &str) -> (String, bool) {
    let mut opts = diffy::DiffOptions::new();
    opts.set_context_len(usize::MAX / 4);
    let patch = opts.create_patch(left, right);
    let mut out = String::new();
    let mut ours: Vec<&str> = Vec::new();
    let mut theirs: Vec<&str> = Vec::new();
    let mut clean = true;
    let flush =
        |out: &mut String, ours: &mut Vec<&str>, theirs: &mut Vec<&str>, clean: &mut bool| {
            if ours.is_empty() && theirs.is_empty() {
                return;
            }
            *clean = false;
            out.push_str(&format!("<<<<<<< {left_label}\n"));
            for l in ours.drain(..) {
                push_line(out, l);
            }
            out.push_str("=======\n");
            for l in theirs.drain(..) {
                push_line(out, l);
            }
            out.push_str(&format!(">>>>>>> {right_label}\n"));
        };
    if patch.hunks().is_empty() {
        return (left.to_string(), true);
    }
    for hunk in patch.hunks() {
        for line in hunk.lines() {
            match line {
                diffy::Line::Context(l) => {
                    flush(&mut out, &mut ours, &mut theirs, &mut clean);
                    push_line(&mut out, l);
                }
                diffy::Line::Delete(l) => ours.push(l),
                diffy::Line::Insert(l) => theirs.push(l),
            }
        }
    }
    flush(&mut out, &mut ours, &mut theirs, &mut clean);
    (out, clean)
}

fn push_line(out: &mut String, l: &str) {
    out.push_str(l);
    if !l.ends_with('\n') {
        out.push('\n');
    }
}

/// diffy labels the sides "ours"/"theirs"; use the real names.
fn relabel(marked: &str, left: &str, right: &str) -> String {
    marked
        .lines()
        .map(|l| {
            if l.starts_with("<<<<<<<") {
                format!("<<<<<<< {left}")
            } else if l.starts_with(">>>>>>>") {
                format!(">>>>>>> {right}")
            } else if l.starts_with("|||||||") {
                "||||||| base".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + if marked.ends_with('\n') { "\n" } else { "" }
}

/// `W2.conflict-CopyB-2021-03-04-2.pdf` → (`W2.pdf`, `CopyB`).
pub fn parse_conflict_name(name: &str) -> Option<(String, String)> {
    let i = name.find(".conflict-")?;
    let stem = &name[..i];
    let rest = &name[i + ".conflict-".len()..];
    // Find the date: the last `-YYYY-MM-DD` in rest.
    let bytes = rest.as_bytes();
    let mut date_at = None;
    for j in (0..rest.len()).rev() {
        if j + 11 <= rest.len() && bytes[j] == b'-' {
            let d = &rest[j + 1..j + 11];
            let db = d.as_bytes();
            if db[4] == b'-'
                && db[7] == b'-'
                && d.chars()
                    .enumerate()
                    .all(|(k, c)| k == 4 || k == 7 || c.is_ascii_digit())
            {
                date_at = Some(j);
                break;
            }
        }
    }
    let j = date_at?;
    let origin = rest[..j].to_string();
    let mut tail = &rest[j + 11..];
    // Optional "-N" uniqueness suffix.
    if let Some(t) = tail.strip_prefix('-') {
        let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 {
            tail = &t[digits..];
        }
    }
    if !(tail.is_empty() || tail.starts_with('.')) || stem.is_empty() {
        return None;
    }
    Some((format!("{stem}{tail}"), origin))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_conflict_names() {
        assert_eq!(
            parse_conflict_name("W2.conflict-CopyB-2021-03-04.pdf"),
            Some(("W2.pdf".into(), "CopyB".into()))
        );
        assert_eq!(
            parse_conflict_name("plan.conflict-Old-Laptop-2023-01-01-2.md"),
            Some(("plan.md".into(), "Old-Laptop".into()))
        );
        assert_eq!(
            parse_conflict_name("README.conflict-x-2021-03-04"),
            Some(("README".into(), "x".into()))
        );
        assert_eq!(parse_conflict_name("notes.txt"), None);
        assert_eq!(parse_conflict_name("a.conflict-nodate.txt"), None);
    }

    #[test]
    fn two_way_marks_only_disagreements() {
        let (out, clean) = two_way("a\nb\nc\n", "a\nB\nc\n", "left", "right");
        assert!(!clean);
        assert_eq!(out, "a\n<<<<<<< left\nb\n=======\nB\n>>>>>>> right\nc\n");
        let (same, clean) = two_way("x\n", "x\n", "l", "r");
        assert!(clean);
        assert_eq!(same, "x\n");
    }
}
