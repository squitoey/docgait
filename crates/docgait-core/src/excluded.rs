//! What a source's scan skipped, and why: so nothing disappears silently behind a `.gitignore`.
//!
//! Walks the source with no filters and reports every top-most entry that is not in the index,
//! without descending into it. The reason is worked out per entry: a built-in exclude, a
//! `CACHEDIR.TAG` folder, the source's own exclude patterns, a symlink, or an ignore file.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use serde::Serialize;

use crate::exclude::{is_cache_dir, DefaultRules, DOCGAIT_IGNORE};
use crate::library::rel_string;
use crate::{scan, Library};

const MAX_ENTRIES: usize = 5000;

#[derive(Serialize, Clone, Debug)]
pub struct ExcludedEntry {
    /// Relative to the source root, '/'-separated.
    pub rel: String,
    pub is_dir: bool,
    /// builtin | cachedir | pattern | symlink | ignore-file
    pub reason: &'static str,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct ExcludedSummary {
    pub source_id: i64,
    pub entries: Vec<ExcludedEntry>,
    /// Count of top-most excluded entries per reason.
    pub by_reason: BTreeMap<String, u64>,
    pub truncated: bool,
}

impl Library {
    /// Rescans the source, then lists what was left out and why.
    pub fn excluded(&self, source_id: i64) -> Result<ExcludedSummary> {
        let source = self.source(source_id)?;
        let root = PathBuf::from(&source.root);
        if !root.is_dir() {
            bail!("{} is offline", source.root);
        }
        scan::scan_source(self, source_id, &|_| {}, &AtomicBool::new(false))?;

        // What the index holds for this source.
        let conn = self.reader()?;
        let folders: HashSet<String> = conn
            .prepare("SELECT rel_path FROM folder WHERE source_id = ?1")?
            .query_map([source_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        let files: HashSet<String> = conn
            .prepare(
                "SELECT CASE d.rel_path WHEN '' THEN f.name ELSE d.rel_path || '/' || f.name END
                   FROM file f JOIN folder d ON d.id = f.folder_id WHERE f.source_id = ?1",
            )?
            .query_map([source_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        drop(conn);

        let rules = DefaultRules::new(&source.includes)?;
        let mut ob = OverrideBuilder::new(&root);
        for pat in &source.excludes {
            ob.add(&format!("!{}", pat.trim_start_matches('!')))?;
        }
        let overrides = ob.build()?;

        let found: Arc<Mutex<Vec<ExcludedEntry>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = found.clone();
        let root_f = root.clone();
        let mut b = WalkBuilder::new(&root);
        b.standard_filters(false)
            .hidden(false)
            .follow_links(false)
            .parents(false);
        b.filter_entry(move |e| {
            let path = e.path();
            if path == root_f {
                return true;
            }
            let Ok(rel) = path.strip_prefix(&root_f) else {
                return false;
            };
            let rel = rel_string(rel);
            let ft = e.file_type();
            let is_symlink = ft.is_some_and(|t| t.is_symlink());
            let is_dir = ft.is_some_and(|t| t.is_dir());
            let indexed = if is_dir {
                folders.contains(&rel)
            } else {
                files.contains(&rel)
            };
            if indexed && !is_symlink {
                return true;
            }
            let reason = if is_symlink {
                "symlink"
            } else if is_dir && is_cache_dir(path) {
                "cachedir"
            } else if rules.excludes(path, is_dir) {
                "builtin"
            } else if overrides.matched(path, is_dir).is_ignore() {
                "pattern"
            } else {
                "ignore-file"
            };
            if let Ok(mut v) = sink.lock() {
                v.push(ExcludedEntry {
                    rel,
                    is_dir,
                    reason,
                });
            }
            false
        });
        for _ in b.build() {}

        let mut entries = std::mem::take(&mut *found.lock().unwrap_or_else(|e| e.into_inner()));
        entries.sort_by(|a, b| a.rel.cmp(&b.rel));
        let mut out = ExcludedSummary {
            source_id,
            ..Default::default()
        };
        for e in &entries {
            *out.by_reason.entry(e.reason.to_string()).or_default() += 1;
        }
        out.truncated = entries.len() > MAX_ENTRIES;
        entries.truncate(MAX_ENTRIES);
        out.entries = entries;
        Ok(out)
    }

    /// Brings back an excluded entry. A built-in exclude is overridden by adding its name to the
    /// source's includes; anything excluded by an ignore file gets a `!` line in the source's
    /// `.docgaitignore` (created if needed, the one file docgait writes in a source, and only on
    /// request). Rescans the source afterwards.
    pub fn include_anyway(&self, source_id: i64, rel: &str, reason: &str) -> Result<()> {
        let source = self.source(source_id)?;
        match reason {
            "builtin" | "cachedir" => {
                let name = Path::new(rel)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| rel.to_string());
                let mut includes = source.includes.clone();
                if !includes.contains(&name) {
                    includes.push(name);
                }
                self.set_source_patterns(source_id, &source.excludes, &includes)?;
            }
            "pattern" => {
                let excludes: Vec<String> = source
                    .excludes
                    .iter()
                    .filter(|p| {
                        let p = p.trim_matches('/');
                        p != rel && !rel.starts_with(&format!("{p}/"))
                    })
                    .cloned()
                    .collect();
                self.set_source_patterns(source_id, &excludes, &source.includes)?;
            }
            "ignore-file" => {
                let file = Path::new(&source.root).join(DOCGAIT_IGNORE);
                let mut text = std::fs::read_to_string(&file).unwrap_or_default();
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&format!("!/{rel}\n"));
                std::fs::write(&file, text)?;
            }
            other => bail!("cannot bring back an entry excluded as {other}"),
        }
        scan::scan_source(self, source_id, &|_| {}, &AtomicBool::new(false))?;
        Ok(())
    }
}
