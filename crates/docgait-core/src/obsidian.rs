//! Obsidian vaults: keep note links working when docgait moves or renames files inside a vault.
//!
//! A vault is a folder containing `.obsidian/`. Links handled:
//! - wikilinks `[[target]]`, `[[target|alias]]`, `[[target#heading]]`, `![[embed]]`, where target
//!   is a name (resolved like Obsidian: same folder first, then the shortest path) or a
//!   vault-relative path, with or without `.md`;
//! - markdown links `[text](path)` / `![alt](path)` with a path relative to the note (or to the
//!   vault root when it starts with `/`), `%20`-encoded or not.
//!
//! Links in moved notes that are relative to the note are rewritten too.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use anyhow::Result;

/// Largest note that gets its links rewritten.
const MAX_NOTE: u64 = 5 * 1024 * 1024;

/// A note whose links change: `path` is where it will be after the moves.
#[derive(Debug, Clone)]
pub struct NoteEdit {
    pub path: PathBuf,
    pub original: String,
    pub updated: String,
}

/// The vault that contains `path`, if any.
pub fn vault_root(path: &Path) -> Option<PathBuf> {
    let mut cur = if path.is_dir() {
        Some(path)
    } else {
        path.parent()
    };
    while let Some(dir) = cur {
        if dir.join(".obsidian").is_dir() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// Every link update needed in `vault` for `moves` (old absolute path → new absolute path, files
/// only), computed before anything moves. Also returns warnings (e.g. ambiguous names).
pub fn plan_link_updates(
    vault: &Path,
    moves: &[(PathBuf, PathBuf)],
) -> Result<(Vec<NoteEdit>, Vec<String>)> {
    let rel = |p: &Path| -> Option<String> {
        p.strip_prefix(vault)
            .ok()
            .map(|r| r.to_string_lossy().replace('\\', "/"))
    };
    let mut moved: HashMap<String, String> = HashMap::new();
    for (from, to) in moves {
        if let (Some(f), Some(t)) = (rel(from), rel(to)) {
            moved.insert(f, t);
        }
    }
    if moved.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    // Every file in the vault (before and after), for resolving names.
    let mut before: Vec<String> = Vec::new();
    for e in ignore::WalkBuilder::new(vault)
        .standard_filters(false)
        .filter_entry(|e| {
            e.file_name() != ".obsidian" && e.file_name() != ".trash" && e.file_name() != ".git"
        })
        .build()
        .flatten()
    {
        if e.file_type().is_some_and(|t| t.is_file()) {
            if let Some(r) = rel(e.path()) {
                before.push(r);
            }
        }
    }
    let after: Vec<String> = before
        .iter()
        .map(|p| moved.get(p).cloned().unwrap_or_else(|| p.clone()))
        .collect();
    let index_before = NameIndex::new(&before);
    let index_after = NameIndex::new(&after);

    let mut edits = Vec::new();
    let warnings = Vec::new();
    for note in before.iter().filter(|p| p.to_lowercase().ends_with(".md")) {
        let abs = vault.join(note);
        if std::fs::metadata(&abs)
            .map(|m| m.len() > MAX_NOTE)
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&abs) else {
            continue;
        };
        let note_after = moved.get(note).cloned().unwrap_or_else(|| note.clone());
        let updated = rewrite_note(
            &text,
            note,
            &note_after,
            &moved,
            &index_before,
            &index_after,
        );
        if updated != text {
            edits.push(NoteEdit {
                path: vault.join(&note_after),
                original: text,
                updated,
            });
        }
    }
    Ok((edits, warnings))
}

/// Vault-relative paths by lower-cased file name, for resolving `[[name]]` links.
struct NameIndex {
    by_name: HashMap<String, Vec<String>>,
    paths: HashMap<String, String>,
}

impl NameIndex {
    fn new(paths: &[String]) -> Self {
        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        let mut all = HashMap::new();
        for p in paths {
            let name = p.rsplit('/').next().unwrap_or(p).to_lowercase();
            by_name.entry(name).or_default().push(p.clone());
            all.insert(p.to_lowercase(), p.clone());
        }
        Self {
            by_name,
            paths: all,
        }
    }

    /// Resolves a wikilink target as Obsidian does, from the note at `from_note`.
    fn resolve(&self, target: &str, from_note: &str) -> Option<String> {
        let t = target.trim().trim_start_matches('/');
        let lower = t.to_lowercase();
        if t.contains('/') {
            for cand in [lower.clone(), format!("{lower}.md")] {
                if let Some(p) = self.paths.get(&cand) {
                    return Some(p.clone());
                }
            }
            return None;
        }
        let note_dir = from_note.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        for cand in [lower.clone(), format!("{lower}.md")] {
            if let Some(list) = self.by_name.get(&cand) {
                if let Some(same) = list
                    .iter()
                    .find(|p| p.rsplit_once('/').map(|(d, _)| d).unwrap_or("") == note_dir)
                {
                    return Some(same.clone());
                }
                return list
                    .iter()
                    .min_by_key(|p| (p.matches('/').count(), p.len()))
                    .cloned();
            }
        }
        None
    }

    fn name_is_unique(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        self.by_name
            .get(&lower)
            .map(|v| v.len() == 1)
            .unwrap_or(false)
            || self
                .by_name
                .get(&format!("{lower}.md"))
                .map(|v| v.len() == 1)
                .unwrap_or(false)
    }
}

fn rewrite_note(
    text: &str,
    note_before: &str,
    note_after: &str,
    moved: &HashMap<String, String>,
    before: &NameIndex,
    after: &NameIndex,
) -> String {
    let text = rewrite_wikilinks(text, note_before, moved, before, after);
    rewrite_markdown_links(&text, note_before, note_after, moved)
}

/// `[[target#sub|alias]]` and `![[...]]`.
fn rewrite_wikilinks(
    text: &str,
    note: &str,
    moved: &HashMap<String, String>,
    before: &NameIndex,
    after: &NameIndex,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        let Some(end_rel) = rest[start + 2..].find("]]") else {
            break;
        };
        let inner = &rest[start + 2..start + 2 + end_rel];
        out.push_str(&rest[..start + 2]);
        out.push_str(&rewrite_wiki_inner(inner, note, moved, before, after));
        out.push_str("]]");
        rest = &rest[start + 2 + end_rel + 2..];
    }
    out.push_str(rest);
    out
}

fn rewrite_wiki_inner(
    inner: &str,
    note: &str,
    moved: &HashMap<String, String>,
    before: &NameIndex,
    after: &NameIndex,
) -> String {
    if inner.contains('\n') {
        return inner.to_string();
    }
    let (target_part, alias) = match inner.find('|') {
        Some(i) => (&inner[..i], &inner[i..]),
        None => (inner, ""),
    };
    let (target, sub) = match target_part.find(['#', '^']) {
        Some(i) => (&target_part[..i], &target_part[i..]),
        None => (target_part, ""),
    };
    if target.trim().is_empty() {
        return inner.to_string();
    }
    let Some(resolved) = before.resolve(target, note) else {
        return inner.to_string();
    };
    let Some(new_path) = moved.get(&resolved) else {
        return inner.to_string();
    };
    let had_md = target.to_lowercase().ends_with(".md");
    let strip_md = |p: &str| -> String {
        if !had_md && p.to_lowercase().ends_with(".md") {
            p[..p.len() - 3].to_string()
        } else {
            p.to_string()
        }
    };
    let new_name = new_path.rsplit('/').next().unwrap_or(new_path);
    let new_target = if !target.contains('/') && after.name_is_unique(new_name) {
        strip_md(new_name)
    } else {
        strip_md(new_path)
    };
    format!("{new_target}{sub}{alias}")
}

/// `[text](path)` and `![alt](path)` with local paths.
fn rewrite_markdown_links(
    text: &str,
    note_before: &str,
    note_after: &str,
    moved: &HashMap<String, String>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut last = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b']' && bytes[i + 1] == b'(' {
            if let Some(close) = text[i + 2..].find(')') {
                let url_part = &text[i + 2..i + 2 + close];
                if let Some(new_url) = rewrite_url(url_part, note_before, note_after, moved) {
                    out.push_str(&text[last..i + 2]);
                    out.push_str(&new_url);
                    last = i + 2 + close;
                }
                i += 2 + close;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&text[last..]);
    out
}

fn rewrite_url(
    url_part: &str,
    note_before: &str,
    note_after: &str,
    moved: &HashMap<String, String>,
) -> Option<String> {
    // Keep an optional title: (path "title")
    let (url, title) = match url_part.find(" \"") {
        Some(i) => (&url_part[..i], &url_part[i..]),
        None => (url_part, ""),
    };
    if url.is_empty()
        || url.contains("://")
        || url.starts_with('#')
        || url.starts_with("mailto:")
        || url.contains(' ')
    {
        return None;
    }
    let (path_enc, anchor) = match url.find('#') {
        Some(i) => (&url[..i], &url[i..]),
        None => (url, ""),
    };
    let encoded = path_enc.contains("%20");
    let path = path_enc.replace("%20", " ");
    let before_dir = note_before.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let target = if let Some(abs) = path.strip_prefix('/') {
        normalize(abs)
    } else {
        normalize(&join(before_dir, &path))
    }?;
    let new_target = moved
        .get(&target)
        .cloned()
        .unwrap_or_else(|| target.clone());
    let note_moved = note_before != note_after;
    if new_target == target && !(note_moved && !path.starts_with('/')) {
        return None;
    }
    let new_path = if path.starts_with('/') {
        format!("/{new_target}")
    } else {
        let after_dir = note_after.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        relative(after_dir, &new_target)
    };
    if new_path == path {
        return None;
    }
    let new_enc = if encoded {
        new_path.replace(' ', "%20")
    } else {
        new_path
    };
    Some(format!("{new_enc}{anchor}{title}"))
}

fn join(dir: &str, rel: &str) -> String {
    if dir.is_empty() {
        rel.to_string()
    } else {
        format!("{dir}/{rel}")
    }
}

/// Resolves `.` and `..`; `None` if it climbs out of the vault.
fn normalize(p: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for c in Path::new(p).components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str()?),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

/// Path to `target` from folder `from_dir` (both vault-relative).
fn relative(from_dir: &str, target: &str) -> String {
    let a: Vec<&str> = from_dir.split('/').filter(|s| !s.is_empty()).collect();
    let b: Vec<&str> = target.split('/').filter(|s| !s.is_empty()).collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<String> = vec!["..".to_string(); a.len() - common];
    parts.extend(b[common..].iter().map(|s| s.to_string()));
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(paths: &[&str]) -> NameIndex {
        NameIndex::new(&paths.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn wikilinks_follow_renames() {
        let moved: HashMap<String, String> = [(
            "Notes/Plan.md".to_string(),
            "Projects/Roadmap.md".to_string(),
        )]
        .into();
        let before = idx(&["Notes/Plan.md", "Daily/today.md"]);
        let after = idx(&["Projects/Roadmap.md", "Daily/today.md"]);
        let text = "See [[Plan]], [[Plan#Goals|the goals]] and ![[Plan]]. Not [[Other]].";
        assert_eq!(
            rewrite_wikilinks(text, "Daily/today.md", &moved, &before, &after),
            "See [[Roadmap]], [[Roadmap#Goals|the goals]] and ![[Roadmap]]. Not [[Other]]."
        );
        // Path-style links stay path-style.
        assert_eq!(
            rewrite_wikilinks("[[Notes/Plan]]", "Daily/today.md", &moved, &before, &after),
            "[[Projects/Roadmap]]"
        );
    }

    #[test]
    fn ambiguous_new_name_uses_a_path() {
        let moved: HashMap<String, String> = [("a/x.md".to_string(), "b/y.md".to_string())].into();
        let before = idx(&["a/x.md", "c/y.md"]);
        let after = idx(&["b/y.md", "c/y.md"]);
        assert_eq!(
            rewrite_wikilinks("[[x]]", "n.md", &moved, &before, &after),
            "[[b/y]]"
        );
    }

    #[test]
    fn markdown_links_relative_and_encoded() {
        let moved: HashMap<String, String> = [(
            "Attachments/scan 1.pdf".to_string(),
            "Archive/2024/scan 1.pdf".to_string(),
        )]
        .into();
        let text = "Scan: [here](../Attachments/scan%201.pdf) and [web](https://x.y/a.pdf)";
        assert_eq!(
            rewrite_markdown_links(text, "Notes/n.md", "Notes/n.md", &moved),
            "Scan: [here](../Archive/2024/scan%201.pdf) and [web](https://x.y/a.pdf)"
        );
        // A moved note's own relative links are rewritten from its new folder.
        let none: HashMap<String, String> = HashMap::new();
        assert_eq!(
            rewrite_markdown_links("![img](pic.png)", "Notes/n.md", "Deep/er/n.md", &none),
            "![img](../../Notes/pic.png)"
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(relative("a/b", "a/c/d.md"), "../c/d.md");
        assert_eq!(relative("", "x.md"), "x.md");
        assert_eq!(normalize("a/../b/./c"), Some("b/c".into()));
        assert_eq!(normalize("../x"), None);
    }
}
