//! Live change detection: watch sources and report which folders need a rescan.
//!
//! Uses the OS facility through `notify` (FSEvents on macOS, inotify on Linux). Events are
//! debounced, mapped to folders inside sources, junk paths (SCM metadata, caches, `.DS_Store`)
//! are dropped, and nested folders are collapsed so each change is rescanned once.
//! Changes made while docgait was closed are caught by the rescan on launch.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use notify::{RecursiveMode, Watcher};

use crate::exclude::DefaultRules;
use crate::library::rel_string;

/// Quiet time before a batch of changes is reported.
const QUIET: Duration = Duration::from_millis(1000);
/// Report at least this often while changes keep coming.
const MAX_WAIT: Duration = Duration::from_secs(5);

/// A folder that changed: rescan it (and below).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Change {
    pub source_id: i64,
    /// Relative to the source root, '/'-separated; "" for the root.
    pub rel: String,
}

/// Keeps watching until dropped.
pub struct SourceWatcher {
    _watcher: notify::RecommendedWatcher,
}

/// Starts watching `sources` (id, root). `on_changes` runs on a background thread.
/// Returns the watcher plus any sources that could not be watched (with the reason).
pub fn watch(
    sources: Vec<(i64, PathBuf)>,
    on_changes: impl Fn(Vec<Change>) + Send + 'static,
) -> Result<(SourceWatcher, Vec<String>)> {
    let (tx, rx) = mpsc::channel::<PathBuf>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            for p in ev.paths {
                let _ = tx.send(p);
            }
        }
    })?;
    let mut problems = Vec::new();
    let mut watched = Vec::new();
    for (id, root) in sources {
        match watcher.watch(&root, RecursiveMode::Recursive) {
            Ok(()) => watched.push((id, root)),
            Err(e) => problems.push(format!("{}: {e}", root.display())),
        }
    }
    let rules = DefaultRules::new(&[])?;
    std::thread::Builder::new()
        .name("watch-debounce".into())
        .spawn(move || debounce(rx, watched, rules, on_changes))?;
    Ok((SourceWatcher { _watcher: watcher }, problems))
}

fn debounce(
    rx: mpsc::Receiver<PathBuf>,
    sources: Vec<(i64, PathBuf)>,
    rules: DefaultRules,
    on_changes: impl Fn(Vec<Change>),
) {
    let mut pending: BTreeSet<Change> = BTreeSet::new();
    let mut first: Option<Instant> = None;
    loop {
        let wait = match first {
            None => Duration::from_secs(3600),
            Some(f) => QUIET.min(MAX_WAIT.saturating_sub(f.elapsed())),
        };
        match rx.recv_timeout(wait) {
            Ok(path) => {
                if let Some(c) = change_for(&sources, &rules, &path) {
                    pending.insert(c);
                    first.get_or_insert_with(Instant::now);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !pending.is_empty() {
                    on_changes(collapse(std::mem::take(&mut pending)));
                }
                first = None;
            }
            // The watcher was dropped.
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// The folder to rescan for a changed path, or `None` for paths outside sources or in junk.
pub fn change_for(sources: &[(i64, PathBuf)], rules: &DefaultRules, path: &Path) -> Option<Change> {
    let (id, root) = sources.iter().find(|(_, r)| path.starts_with(r))?;
    let rel = path.strip_prefix(root).ok()?;
    // Ignore anything in or under a default-excluded folder, and junk files.
    let mut acc = root.clone();
    for comp in rel.components() {
        acc.push(comp);
        let is_last = acc == path;
        let is_dir = !is_last || path.is_dir();
        if rules.excludes(&acc, is_dir) {
            return None;
        }
    }
    let folder = if path.is_dir() {
        rel.to_path_buf()
    } else {
        rel.parent().map(Path::to_path_buf).unwrap_or_default()
    };
    Some(Change {
        source_id: *id,
        rel: rel_string(&folder),
    })
}

/// Drops changes already covered by a change to an ancestor folder in the same source.
pub fn collapse(changes: BTreeSet<Change>) -> Vec<Change> {
    let mut out: Vec<Change> = Vec::new();
    // BTreeSet order puts "a" before "a/b", so ancestors come first.
    for c in changes {
        let covered = out.iter().any(|o| {
            o.source_id == c.source_id
                && (o.rel.is_empty() || c.rel == o.rel || c.rel.starts_with(&format!("{}/", o.rel)))
        });
        if !covered {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(id: i64, rel: &str) -> Change {
        Change {
            source_id: id,
            rel: rel.into(),
        }
    }

    #[test]
    fn collapses_nested_folders() {
        let set: BTreeSet<Change> = [
            ch(1, "a/b"),
            ch(1, "a"),
            ch(1, "ab"),
            ch(2, "a/b/c"),
            ch(2, "x"),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            collapse(set),
            vec![ch(1, "a"), ch(1, "ab"), ch(2, "a/b/c"), ch(2, "x")]
        );
        let set: BTreeSet<Change> = [ch(1, ""), ch(1, "a")].into_iter().collect();
        assert_eq!(collapse(set), vec![ch(1, "")]);
    }

    #[test]
    fn maps_paths_and_drops_junk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("docs/sub")).unwrap();
        let sources = vec![(7, root.clone())];
        let rules = DefaultRules::new(&[]).unwrap();
        assert_eq!(
            change_for(&sources, &rules, &root.join("docs/a.txt")),
            Some(ch(7, "docs"))
        );
        assert_eq!(
            change_for(&sources, &rules, &root.join("docs/sub")),
            Some(ch(7, "docs/sub"))
        );
        assert_eq!(
            change_for(&sources, &rules, &root.join("top.txt")),
            Some(ch(7, ""))
        );
        assert_eq!(
            change_for(&sources, &rules, &root.join("repo/.git/index")),
            None
        );
        assert_eq!(
            change_for(&sources, &rules, &root.join("docs/.DS_Store")),
            None
        );
        assert_eq!(
            change_for(&sources, &rules, &root.join("app/node_modules/x/y.js")),
            None
        );
        assert_eq!(
            change_for(&sources, &rules, Path::new("/elsewhere/file")),
            None
        );
    }
}
