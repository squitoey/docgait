//! Organise: move, rename, create folders, trash, and rule-based "organise by template". Every
//! action is planned first (a dry run), then carried out as one journaled, undoable batch.
//!
//! Moves inside a source update the index in place, so a file keeps its id (and its tags,
//! hashes and anything extracted later). Inside an Obsidian vault, links in notes are updated to
//! follow moved files (see `obsidian.rs`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::library::{join_rel, rel_string};
use crate::{db, fsops, kind, obsidian, scan, trash, Library, Source};

#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum OrganiseOp {
    /// Move files and folders into a folder (names made unique if needed).
    Move {
        #[serde(default)]
        file_ids: Vec<i64>,
        #[serde(default)]
        folder_ids: Vec<i64>,
        dest_folder_id: i64,
    },
    /// Rename one file or folder (fails if the name is taken).
    Rename {
        file_id: Option<i64>,
        folder_id: Option<i64>,
        name: String,
    },
    /// Create a folder.
    Mkdir { parent_folder_id: i64, name: String },
    /// Move files and folders to the trash.
    Trash {
        #[serde(default)]
        file_ids: Vec<i64>,
        #[serde(default)]
        folder_ids: Vec<i64>,
    },
}

#[derive(Serialize, Clone, Debug)]
pub struct PlannedStep {
    /// mkdir | move | trash
    pub op: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub is_dir: bool,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct OrganisePlan {
    pub steps: Vec<PlannedStep>,
    /// Obsidian notes whose links will be updated.
    pub notes_to_update: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct OrganiseResult {
    pub batch_id: i64,
    pub moved: u64,
    pub created: u64,
    pub trashed: u64,
    pub notes_updated: u64,
    pub failed: u64,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug)]
enum Step {
    Mkdir(PathBuf),
    Move {
        from: PathBuf,
        to: PathBuf,
        is_dir: bool,
    },
    Trash {
        path: PathBuf,
        is_dir: bool,
    },
}

impl Step {
    fn view(&self) -> PlannedStep {
        let s = |p: &Path| Some(p.to_string_lossy().to_string());
        match self {
            Step::Mkdir(p) => PlannedStep {
                op: "mkdir".into(),
                from: None,
                to: s(p),
                is_dir: true,
            },
            Step::Move { from, to, is_dir } => PlannedStep {
                op: "move".into(),
                from: s(from),
                to: s(to),
                is_dir: *is_dir,
            },
            Step::Trash { path, is_dir } => PlannedStep {
                op: "trash".into(),
                from: s(path),
                to: None,
                is_dir: *is_dir,
            },
        }
    }
}

struct Planned {
    steps: Vec<Step>,
    notes: Vec<obsidian::NoteEdit>,
    warnings: Vec<String>,
}

impl Library {
    /// Dry run of `ops`: what would happen, which notes would be updated, warnings.
    pub fn plan_organise(&self, ops: &[OrganiseOp]) -> Result<OrganisePlan> {
        let p = self.plan_steps(ops)?;
        Ok(OrganisePlan {
            steps: p.steps.iter().map(Step::view).collect(),
            notes_to_update: p
                .notes
                .iter()
                .map(|n| n.path.to_string_lossy().to_string())
                .collect(),
            warnings: p.warnings,
        })
    }

    /// Carries out `ops` as one undoable batch.
    pub fn organise(&self, ops: &[OrganiseOp], description: &str) -> Result<OrganiseResult> {
        let p = self.plan_steps(ops)?;
        self.run_steps(p, description)
    }

    /// Where each file would go when organised by `template` under `dest_folder_id`.
    /// Tokens: {year} {month} {day} (modified date), {kind}, {ext}, {tag}, {folder}, {source}.
    pub fn preview_template(
        &self,
        file_ids: &[i64],
        dest_folder_id: i64,
        template: &str,
    ) -> Result<OrganisePlan> {
        let p = self.template_steps(file_ids, dest_folder_id, template)?;
        Ok(OrganisePlan {
            steps: p.steps.iter().map(Step::view).collect(),
            notes_to_update: p
                .notes
                .iter()
                .map(|n| n.path.to_string_lossy().to_string())
                .collect(),
            warnings: p.warnings,
        })
    }

    pub fn organise_by_template(
        &self,
        file_ids: &[i64],
        dest_folder_id: i64,
        template: &str,
    ) -> Result<OrganiseResult> {
        let p = self.template_steps(file_ids, dest_folder_id, template)?;
        self.run_steps(
            p,
            &format!("Organise {} files by {template}", file_ids.len()),
        )
    }

    /// All present files under a folder (for "organise this folder").
    pub fn file_ids_under(&self, folder_id: i64) -> Result<Vec<i64>> {
        let f = self.folder(folder_id)?;
        let conn = self.reader()?;
        let mut stmt = conn.prepare(
            "SELECT f.id FROM file f JOIN folder d ON d.id = f.folder_id
              WHERE f.source_id = ?1 AND f.state = 'present' AND f.kind <> 'package'
                AND (?2 = '' OR d.rel_path = ?2 OR substr(d.rel_path, 1, length(?2) + 1) = ?2 || '/')",
        )?;
        let ids = stmt
            .query_map(params![f.source_id, f.rel_path], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(ids)
    }

    fn template_steps(
        &self,
        file_ids: &[i64],
        dest_folder_id: i64,
        template: &str,
    ) -> Result<Planned> {
        let dest = self.folder_path(dest_folder_id)?;
        self.check_writable(&dest)?;
        let mut taken: HashSet<PathBuf> = HashSet::new();
        let mut steps = Vec::new();
        let mut mkdirs: Vec<PathBuf> = Vec::new();
        for &id in file_ids {
            let from = self.file_path(id)?;
            let info = self.file_facts(id)?;
            let rel_dir = render_template(template, &info);
            let dir = join_rel(&dest, &rel_dir);
            let name = from
                .file_name()
                .context("file without a name")?
                .to_string_lossy()
                .to_string();
            let mut to = dir.join(&name);
            if to == from {
                continue;
            }
            to = unique_path(&to, &taken, &from);
            taken.insert(to.clone());
            for d in missing_dirs(&dir) {
                if !mkdirs.contains(&d) {
                    mkdirs.push(d);
                }
            }
            steps.push(Step::Move {
                from,
                to,
                is_dir: false,
            });
        }
        mkdirs.sort();
        let mut all: Vec<Step> = mkdirs.into_iter().map(Step::Mkdir).collect();
        all.extend(steps);
        self.with_vault_links(all)
    }

    fn file_facts(&self, id: i64) -> Result<FileFacts> {
        let conn = self.reader()?;
        let (name, ext, kind, mtime, folder_name, root): (String, String, String, i64, String, String) = conn
            .query_row(
                "SELECT f.name, f.ext, f.kind, f.mtime_ns, d.name, s.root FROM file f
                   JOIN folder d ON d.id = f.folder_id JOIN source s ON s.id = f.source_id WHERE f.id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )?;
        let tag: Option<String> = conn
            .query_row(
                "SELECT t.name FROM file_tag ft JOIN tag t ON t.id = ft.tag_id WHERE ft.file_id = ?1
                  ORDER BY t.name COLLATE NOCASE LIMIT 1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let _ = name;
        Ok(FileFacts {
            ext,
            kind,
            mtime_ns: mtime,
            folder: folder_name,
            source: Path::new(&root)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            tag,
        })
    }

    fn plan_steps(&self, ops: &[OrganiseOp]) -> Result<Planned> {
        let mut steps: Vec<Step> = Vec::new();
        let mut taken: HashSet<PathBuf> = HashSet::new();
        for op in ops {
            match op {
                OrganiseOp::Mkdir {
                    parent_folder_id,
                    name,
                } => {
                    check_name(name)?;
                    let parent = self.folder_path(*parent_folder_id)?;
                    self.check_writable(&parent)?;
                    let p = parent.join(name);
                    if p.exists() || taken.contains(&p) {
                        bail!("{} already exists", p.display());
                    }
                    taken.insert(p.clone());
                    steps.push(Step::Mkdir(p));
                }
                OrganiseOp::Rename {
                    file_id,
                    folder_id,
                    name,
                } => {
                    check_name(name)?;
                    let (from, is_dir) = match (file_id, folder_id) {
                        (Some(f), None) => (self.file_path(*f)?, false),
                        (None, Some(d)) => {
                            let folder = self.folder(*d)?;
                            if folder.rel_path.is_empty() {
                                bail!("a source's own folder cannot be renamed here");
                            }
                            (self.folder_path(*d)?, true)
                        }
                        _ => bail!("rename one file or one folder"),
                    };
                    self.check_writable(&from)?;
                    let to = from.with_file_name(name);
                    if to == from {
                        continue;
                    }
                    // Allow changing only the case of a name on case-insensitive volumes.
                    let case_only = fsops::case_insensitive_names()
                        && to.to_string_lossy().to_lowercase()
                            == from.to_string_lossy().to_lowercase();
                    if (to.exists() && !case_only) || taken.contains(&to) {
                        bail!("{} already exists", to.display());
                    }
                    taken.insert(to.clone());
                    steps.push(Step::Move { from, to, is_dir });
                }
                OrganiseOp::Move {
                    file_ids,
                    folder_ids,
                    dest_folder_id,
                } => {
                    let dest = self.folder_path(*dest_folder_id)?;
                    self.check_writable(&dest)?;
                    let mut items: Vec<(PathBuf, bool)> = Vec::new();
                    for id in file_ids {
                        items.push((self.file_path(*id)?, false));
                    }
                    for id in folder_ids {
                        let folder = self.folder(*id)?;
                        if folder.rel_path.is_empty() {
                            bail!("a source's own folder cannot be moved here");
                        }
                        let p = self.folder_path(*id)?;
                        if dest.starts_with(&p) {
                            bail!("cannot move {} into itself", p.display());
                        }
                        items.push((p, true));
                    }
                    for (from, is_dir) in items {
                        self.check_writable(&from)?;
                        let name = from.file_name().context("no name")?;
                        if from.parent() == Some(dest.as_path()) {
                            continue; // already there
                        }
                        let to = unique_path(&dest.join(name), &taken, &from);
                        taken.insert(to.clone());
                        steps.push(Step::Move { from, to, is_dir });
                    }
                }
                OrganiseOp::Trash {
                    file_ids,
                    folder_ids,
                } => {
                    for id in file_ids {
                        let p = self.file_path(*id)?;
                        self.check_writable(&p)?;
                        steps.push(Step::Trash {
                            path: p,
                            is_dir: false,
                        });
                    }
                    for id in folder_ids {
                        let folder = self.folder(*id)?;
                        if folder.rel_path.is_empty() {
                            bail!("a source's own folder cannot be trashed here; remove the source instead");
                        }
                        let p = self.folder_path(*id)?;
                        self.check_writable(&p)?;
                        steps.push(Step::Trash {
                            path: p,
                            is_dir: true,
                        });
                    }
                }
            }
        }
        self.with_vault_links(steps)
    }

    /// Adds Obsidian link updates (and warnings) for moves inside vaults.
    fn with_vault_links(&self, steps: Vec<Step>) -> Result<Planned> {
        let mut warnings = Vec::new();
        let mut by_vault: HashMap<PathBuf, Vec<(PathBuf, PathBuf)>> = HashMap::new();
        for s in &steps {
            match s {
                Step::Move { from, to, is_dir } => {
                    let vf = obsidian::vault_root(from);
                    let vt = obsidian::vault_root(to);
                    match (&vf, &vt) {
                        (Some(a), Some(b)) if a == b => {
                            for (f, t) in expand_move(from, to, *is_dir) {
                                by_vault.entry(a.clone()).or_default().push((f, t));
                            }
                        }
                        (Some(a), _) => warnings.push(format!(
                            "{} leaves the Obsidian vault {}; links to it in notes will break",
                            from.display(),
                            a.display()
                        )),
                        _ => {}
                    }
                }
                Step::Trash { path, .. } => {
                    if let Some(v) = obsidian::vault_root(path) {
                        warnings.push(format!(
                            "{} is in the Obsidian vault {}; links to it will break",
                            path.display(),
                            v.display()
                        ));
                    }
                }
                Step::Mkdir(_) => {}
            }
        }
        let mut notes = Vec::new();
        for (vault, moves) in by_vault {
            let (edits, warn) = obsidian::plan_link_updates(&vault, &moves)?;
            notes.extend(edits);
            warnings.extend(warn);
        }
        Ok(Planned {
            steps,
            notes,
            warnings,
        })
    }

    /// Offline sources are read-only, and paths must be inside a source of this library.
    fn check_writable(&self, path: &Path) -> Result<()> {
        let src = self.source_containing(path)?.with_context(|| {
            format!(
                "{} is not inside any source of this library",
                path.display()
            )
        })?;
        if !src.online {
            bail!("{} is offline (read-only)", src.root);
        }
        if is_protected(path) {
            bail!("{} is a protected location", path.display());
        }
        Ok(())
    }

    pub(crate) fn source_containing(&self, path: &Path) -> Result<Option<Source>> {
        Ok(self
            .sources()?
            .into_iter()
            .filter(|s| path.starts_with(&s.root))
            .max_by_key(|s| s.root.len()))
    }

    fn run_steps(&self, p: Planned, description: &str) -> Result<OrganiseResult> {
        if p.steps.is_empty() {
            bail!("nothing to do");
        }
        // Scope for undo's index refresh: the deepest folder that contains every touched path,
        // when they are all in one source.
        let touched: Vec<&Path> = p
            .steps
            .iter()
            .flat_map(|s| match s {
                Step::Mkdir(x) => vec![x.as_path()],
                Step::Move { from, to, .. } => vec![from.as_path(), to.as_path()],
                Step::Trash { path, .. } => vec![path.as_path()],
            })
            .collect();
        let scope = self.common_scope(&touched)?;
        let batch_id = {
            let conn = self.writer();
            conn.execute(
                "INSERT INTO batch (kind, created_at, description, scope_source_id, scope_rel)
                 VALUES ('organise', ?1, ?2, ?3, ?4)",
                params![
                    db::now_secs(),
                    description,
                    scope.as_ref().map(|s| s.0),
                    scope.as_ref().map(|s| s.1.clone())
                ],
            )?;
            conn.last_insert_rowid()
        };
        let mut res = OrganiseResult {
            batch_id,
            ..Default::default()
        };
        for step in &p.steps {
            let outcome = (|| -> Result<()> {
                match step {
                    Step::Mkdir(dir) => {
                        std::fs::create_dir(dir)
                            .with_context(|| format!("cannot create {}", dir.display()))?;
                        self.journal(batch_id, "mkdir", None, Some(dir), 0, None, None)?;
                        let _ = self.reindex_mkdir(dir);
                        res.created += 1;
                    }
                    Step::Move { from, to, is_dir } => {
                        if let Some(parent) = to.parent() {
                            if !parent.exists() {
                                bail!("{} does not exist", parent.display());
                            }
                        }
                        let case_only = from.to_string_lossy().to_lowercase()
                            == to.to_string_lossy().to_lowercase();
                        if *is_dir {
                            if to.exists() && !case_only {
                                bail!("{} already exists", to.display());
                            }
                            std::fs::rename(from, to).with_context(|| {
                                format!(
                                    "cannot move {} (moving folders between drives isn't supported yet)",
                                    from.display()
                                )
                            })?;
                        } else if case_only {
                            std::fs::rename(from, to)?;
                        } else {
                            fsops::move_file(from, to)?;
                        }
                        self.journal(batch_id, "rename", Some(from), Some(to), 0, None, None)?;
                        let _ = self.reindex_move(from, to);
                        res.moved += 1;
                    }
                    Step::Trash { path, .. } => {
                        let at = trash::trash(path)?;
                        self.journal(batch_id, "trash", Some(path), at.as_deref(), 0, None, None)?;
                        let _ = self.reindex_removed(path);
                        res.trashed += 1;
                    }
                }
                Ok(())
            })();
            if let Err(e) = outcome {
                res.failed += 1;
                if res.errors.len() < 20 {
                    res.errors.push(format!("{e:#}"));
                }
            }
        }
        // Notes are edited last, at their (possibly new) locations.
        let backups = self.data_dir.join("backups").join(batch_id.to_string());
        for (n, edit) in p.notes.iter().enumerate() {
            let outcome = (|| -> Result<()> {
                let current = std::fs::read_to_string(&edit.path)?;
                if current != edit.original {
                    bail!(
                        "{} changed while organising; links not updated",
                        edit.path.display()
                    );
                }
                std::fs::create_dir_all(&backups)?;
                let backup = backups.join(format!("{n}.md"));
                std::fs::write(&backup, &edit.original)?;
                replace_contents(&edit.path, &edit.updated)?;
                let h = blake3::hash(edit.updated.as_bytes());
                self.journal(
                    batch_id,
                    "edit",
                    Some(&backup),
                    Some(&edit.path),
                    0,
                    Some(h.as_bytes()),
                    None,
                )?;
                res.notes_updated += 1;
                Ok(())
            })();
            if let Err(e) = outcome {
                res.failed += 1;
                if res.errors.len() < 20 {
                    res.errors.push(format!("{e:#}"));
                }
            }
        }
        let state = if res.failed == 0 { "done" } else { "partial" };
        self.writer().execute(
            "UPDATE batch SET state = ?2 WHERE id = ?1",
            params![batch_id, state],
        )?;
        Ok(res)
    }

    fn common_scope(&self, paths: &[&Path]) -> Result<Option<(i64, String)>> {
        let mut src: Option<Source> = None;
        for p in paths {
            let s = self.source_containing(p)?;
            match (&src, s) {
                (None, Some(s)) => src = Some(s),
                (Some(a), Some(b)) if a.id == b.id => {}
                _ => return Ok(None),
            }
        }
        let Some(src) = src else { return Ok(None) };
        let root = PathBuf::from(&src.root);
        let mut common: Option<PathBuf> = None;
        for p in paths {
            let dir = p.parent().unwrap_or(p).to_path_buf();
            common = Some(match common {
                None => dir,
                Some(c) => {
                    let mut c = c;
                    while !dir.starts_with(&c) {
                        if !c.pop() {
                            break;
                        }
                    }
                    c
                }
            });
        }
        let common = common.unwrap_or_else(|| root.clone());
        let rel = common
            .strip_prefix(&root)
            .map(rel_string)
            .unwrap_or_default();
        Ok(Some((src.id, rel)))
    }

    /// Brings the index in line after `from` was renamed/moved to `to`, in place when both are in
    /// the same source (ids, tags and hashes are kept), otherwise by rescanning both places.
    pub(crate) fn reindex_move(&self, from: &Path, to: &Path) -> Result<()> {
        let (Some(sf), Some(st)) = (self.source_containing(from)?, self.source_containing(to)?)
        else {
            return Ok(());
        };
        let rel =
            |s: &Source, p: &Path| p.strip_prefix(&s.root).map(rel_string).unwrap_or_default();
        let (from_rel, to_rel) = (rel(&sf, from), rel(&st, to));
        if sf.id != st.id {
            let parent = |r: &str| {
                r.rsplit_once('/')
                    .map(|(a, _)| a.to_string())
                    .unwrap_or_default()
            };
            scan::scan_folder(
                self,
                sf.id,
                &parent(&from_rel),
                &|_| {},
                &AtomicBool::new(false),
            )?;
            scan::scan_folder(
                self,
                st.id,
                &parent(&to_rel),
                &|_| {},
                &AtomicBool::new(false),
            )?;
            return Ok(());
        }
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        let split = |r: &str| match r.rsplit_once('/') {
            Some((d, n)) => (d.to_string(), n.to_string()),
            None => (String::new(), r.to_string()),
        };
        let (from_dir, from_name) = split(&from_rel);
        let (to_dir, to_name) = split(&to_rel);
        let old_parent = folder_id(&tx, sf.id, &from_dir)?;
        let new_parent = ensure_folder_row(&tx, sf.id, &to_dir)?;
        let folder_row: Option<i64> = tx
            .query_row(
                "SELECT id FROM folder WHERE source_id = ?1 AND rel_path = ?2",
                params![sf.id, from_rel],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(fid) = folder_row {
            tx.execute(
                "UPDATE folder SET parent_id = ?2, name = ?3 WHERE id = ?1",
                params![fid, new_parent, to_name],
            )?;
            // Every folder at or below it gets the new path prefix.
            tx.execute(
                "UPDATE folder SET rel_path = ?3 || substr(rel_path, length(?2) + 1)
                  WHERE source_id = ?1 AND (rel_path = ?2 OR substr(rel_path, 1, length(?2) + 1) = ?2 || '/')",
                params![sf.id, from_rel, to_rel],
            )?;
            // Their files' search entries carry the folder path: refresh them.
            tx.execute(
                "DELETE FROM file_fts WHERE rowid IN (
                   SELECT f.id FROM file f JOIN folder d ON d.id = f.folder_id
                    WHERE d.source_id = ?1 AND (d.rel_path = ?2 OR substr(d.rel_path, 1, length(?2) + 1) = ?2 || '/'))",
                params![sf.id, to_rel],
            )?;
            tx.execute(
                "INSERT INTO file_fts (rowid, name, path)
                   SELECT f.id, f.name, d.rel_path FROM file f JOIN folder d ON d.id = f.folder_id
                    WHERE d.source_id = ?1 AND (d.rel_path = ?2 OR substr(d.rel_path, 1, length(?2) + 1) = ?2 || '/')",
                params![sf.id, to_rel],
            )?;
        } else {
            let ext = kind::extension(&to_name);
            let k = kind::kind_of(&ext);
            tx.execute(
                "UPDATE file SET folder_id = ?3, name = ?4, ext = ?5, kind = CASE kind WHEN 'package' THEN kind ELSE ?6 END
                  WHERE folder_id = ?1 AND name = ?2",
                params![old_parent, from_name, new_parent, to_name, ext, k],
            )?;
        }
        refresh_counts(&tx, &[old_parent, Some(new_parent)])?;
        tx.commit()?;
        Ok(())
    }

    fn reindex_mkdir(&self, dir: &Path) -> Result<()> {
        let Some(src) = self.source_containing(dir)? else {
            return Ok(());
        };
        let rel = dir
            .strip_prefix(&src.root)
            .map(rel_string)
            .unwrap_or_default();
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        let id = ensure_folder_row(&tx, src.id, &rel)?;
        let parent: Option<i64> =
            tx.query_row("SELECT parent_id FROM folder WHERE id = ?1", [id], |r| {
                r.get(0)
            })?;
        refresh_counts(&tx, &[parent])?;
        tx.commit()?;
        Ok(())
    }

    fn reindex_removed(&self, path: &Path) -> Result<()> {
        let Some(src) = self.source_containing(path)? else {
            return Ok(());
        };
        let rel = path
            .strip_prefix(&src.root)
            .map(rel_string)
            .unwrap_or_default();
        let parent = rel
            .rsplit_once('/')
            .map(|(a, _)| a.to_string())
            .unwrap_or_default();
        scan::scan_folder(self, src.id, &parent, &|_| {}, &AtomicBool::new(false))?;
        Ok(())
    }
}

struct FileFacts {
    ext: String,
    kind: String,
    mtime_ns: i64,
    folder: String,
    source: String,
    tag: Option<String>,
}

/// Renders a folder template like `{kind}/{year}/{month}` for one file.
fn render_template(template: &str, f: &FileFacts) -> String {
    let date = crate::import::ymd(f.mtime_ns); // YYYY-MM-DD
    let (y, m, d) = (&date[0..4], &date[5..7], &date[8..10]);
    let out = template
        .replace("{year}", y)
        .replace("{month}", m)
        .replace("{day}", d)
        .replace("{kind}", &f.kind)
        .replace(
            "{ext}",
            if f.ext.is_empty() {
                "no-extension"
            } else {
                &f.ext
            },
        )
        .replace("{tag}", f.tag.as_deref().unwrap_or("untagged"))
        .replace(
            "{folder}",
            if f.folder.is_empty() {
                "root"
            } else {
                &f.folder
            },
        )
        .replace("{source}", &f.source);
    // Each segment must be a plain name.
    out.split(['/', '\\'])
        .map(|seg| seg.trim())
        .filter(|seg| !seg.is_empty() && *seg != "." && *seg != "..")
        .collect::<Vec<_>>()
        .join("/")
}

/// Locations docgait never changes: app libraries managed by other programs, system folders.
fn is_protected(path: &Path) -> bool {
    let s = path.to_string_lossy();
    path.components().any(|c| {
        let n = c.as_os_str().to_string_lossy().to_lowercase();
        n.ends_with(".photoslibrary") || n.ends_with(".musiclibrary") || n.ends_with(".aplibrary")
    }) || ["/System", "/usr", "/bin", "/sbin", "/etc", "C:\\Windows"]
        .iter()
        .any(|p| s.starts_with(p))
}

fn check_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.contains(['/', '\\']) || name == "." || name == ".." {
        bail!("{name:?} is not a valid name");
    }
    Ok(())
}

/// `dir/name.ext`, or `dir/name (2).ext` etc. when taken (on disk or by another planned step).
fn unique_path(want: &Path, taken: &HashSet<PathBuf>, from: &Path) -> PathBuf {
    let free = |p: &Path| p == from || (!p.exists() && !taken.contains(p));
    if free(want) {
        return want.to_path_buf();
    }
    let name = want
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name.as_str(), ""),
    };
    for n in 2.. {
        let p = want.with_file_name(format!("{stem} ({n}){ext}"));
        if free(&p) {
            return p;
        }
    }
    unreachable!()
}

/// Folders from the first missing ancestor down to `dir`.
fn missing_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if d.exists() {
            break;
        }
        out.push(d.to_path_buf());
        cur = d.parent();
    }
    out.reverse();
    out
}

/// For link updates: every file a move carries (one for a file; all files inside for a folder).
fn expand_move(from: &Path, to: &Path, is_dir: bool) -> Vec<(PathBuf, PathBuf)> {
    if !is_dir {
        return vec![(from.to_path_buf(), to.to_path_buf())];
    }
    let mut out = Vec::new();
    for e in ignore::WalkBuilder::new(from)
        .standard_filters(false)
        .build()
        .flatten()
    {
        if e.file_type().is_some_and(|t| t.is_file()) {
            if let Ok(rel) = e.path().strip_prefix(from) {
                out.push((e.path().to_path_buf(), to.join(rel)));
            }
        }
    }
    out
}

/// Replaces a file's contents safely (temp file, then rename over it).
fn replace_contents(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_file_name(format!(
        ".{}.docgait-tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        std::process::id()
    ));
    std::fs::write(&tmp, text)?;
    if let Ok(md) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, md.permissions());
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn folder_id(conn: &Connection, source_id: i64, rel: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM folder WHERE source_id = ?1 AND rel_path = ?2",
            params![source_id, rel],
            |r| r.get(0),
        )
        .optional()?)
}

/// The folder row for `rel`, creating it (and missing parents) when needed.
fn ensure_folder_row(conn: &Connection, source_id: i64, rel: &str) -> Result<i64> {
    if let Some(id) = folder_id(conn, source_id, rel)? {
        return Ok(id);
    }
    let gen: i64 = conn.query_row(
        "SELECT scan_gen FROM source WHERE id = ?1",
        [source_id],
        |r| r.get(0),
    )?;
    let mut map = HashMap::new();
    scan::ensure_folder(conn, source_id, gen, &mut map, rel)
}

fn refresh_counts(conn: &Connection, folders: &[Option<i64>]) -> Result<()> {
    for id in folders.iter().flatten() {
        conn.execute(
            "UPDATE folder SET
               file_count   = (SELECT COUNT(*) FROM file f WHERE f.folder_id = folder.id AND f.state = 'present'),
               total_size   = (SELECT COALESCE(SUM(size), 0) FROM file f WHERE f.folder_id = folder.id AND f.state = 'present'),
               folder_count = (SELECT COUNT(*) FROM folder c WHERE c.parent_id = folder.id AND c.state = 'present')
             WHERE id = ?1",
            [id],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates() {
        let f = FileFacts {
            ext: "pdf".into(),
            kind: "document".into(),
            mtime_ns: 1_614_816_000 * 1_000_000_000,
            folder: "Inbox".into(),
            source: "OldLaptop".into(),
            tag: None,
        };
        assert_eq!(
            render_template("{kind}/{year}/{month}", &f),
            "document/2021/03"
        );
        assert_eq!(render_template("{tag}/{ext}", &f), "untagged/pdf");
        assert_eq!(render_template("../{folder}//x", &f), "Inbox/x");
    }

    #[test]
    fn unique_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let mut taken = HashSet::new();
        let p = unique_path(&dir.path().join("a.txt"), &taken, Path::new("/elsewhere"));
        assert_eq!(p, dir.path().join("a (2).txt"));
        taken.insert(p);
        let p = unique_path(&dir.path().join("a.txt"), &taken, Path::new("/elsewhere"));
        assert_eq!(p, dir.path().join("a (3).txt"));
    }
}
