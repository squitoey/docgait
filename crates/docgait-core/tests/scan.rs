//! End-to-end: build a real tree on disk, scan it, check what the index holds.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use docgait_core::{scan, App, Library, Role, SortBy};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn setup() -> (tempfile::TempDir, tempfile::TempDir, Library) {
    let data = tempfile::tempdir().unwrap();
    let tree = tempfile::tempdir().unwrap();
    let app = App::open(data.path()).unwrap();
    let info = app.default_library().unwrap();
    let lib = app.open_library(info.id).unwrap();
    (data, tree, lib)
}

fn scan_it(lib: &Library, id: i64) -> docgait_core::ScanSummary {
    scan::scan_source(lib, id, &|_| {}, &AtomicBool::new(false)).unwrap()
}

/// Every indexed file as a relative path.
fn indexed(lib: &Library, folder_id: i64, prefix: &str, out: &mut BTreeSet<String>) {
    let l = lib.list(folder_id, SortBy::Name, None, 5000).unwrap();
    for f in &l.files {
        if !f.missing {
            out.insert(format!("{prefix}{}", f.name));
        }
    }
    for d in &l.folders {
        indexed(lib, d.id, &format!("{prefix}{}/", d.name), out);
    }
}

fn all_files(lib: &Library, source_id: i64) -> BTreeSet<String> {
    let s = lib.source(source_id).unwrap();
    let mut out = BTreeSet::new();
    indexed(lib, s.root_folder_id.unwrap(), "", &mut out);
    out
}

#[test]
fn indexes_working_files_and_skips_junk() {
    let (_d, tree, lib) = setup();
    let t = tree.path();
    write(t, "docs/report.pdf", "pdf");
    write(t, "docs/.DS_Store", "junk");
    write(t, "docs/~$report.docx", "lock");
    write(t, "Thumbs.db", "junk");
    write(t, ".bashrc", "keep dotfiles");
    // A repository: working files kept, metadata and dependencies skipped.
    write(t, "code/app/.git/HEAD", "ref");
    write(t, "code/app/.hg/store", "x");
    write(t, "code/app/node_modules/lib/index.js", "x");
    write(t, "code/app/src/main.rs", "fn main() {}");
    write(t, "code/app/.gitignore", "secret.env\nbuild/\n");
    write(t, "code/app/secret.env", "x");
    write(t, "code/app/build/out.bin", "x");
    // Rust target only skipped next to Cargo.toml; a folder merely named target is kept.
    write(t, "code/app/Cargo.toml", "[package]");
    write(t, "code/app/target/debug/app", "x");
    write(t, "archery/target/scores.txt", "keep");
    // Python venv only skipped when it really is one.
    write(t, "py/venv/pyvenv.cfg", "home = /usr");
    write(t, "py/venv/lib/x.py", "x");
    write(t, "notes/venv/plan.md", "keep");
    // Cache directory tag.
    write(
        t,
        "cache/CACHEDIR.TAG",
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    );
    write(t, "cache/blob", "x");
    // .docgaitignore can bring back what .gitignore excluded.
    write(t, "code/app/.docgaitignore", "!secret.env\n");

    let s = lib.add_source(t, Role::Import, None).unwrap();
    let sum = scan_it(&lib, s.id);
    assert!(!sum.cancelled);

    let got = all_files(&lib, s.id);
    let want: BTreeSet<String> = [
        ".bashrc",
        "docs/report.pdf",
        "code/app/src/main.rs",
        "code/app/.gitignore",
        "code/app/.docgaitignore",
        "code/app/secret.env",
        "code/app/Cargo.toml",
        "archery/target/scores.txt",
        "notes/venv/plan.md",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(got, want);
}

#[test]
fn ignore_files_can_be_turned_off() {
    let (_d, tree, lib) = setup();
    let t = tree.path();
    write(t, ".gitignore", "*.log\n");
    write(t, "app.log", "x");
    let s = lib.add_source(t, Role::Import, None).unwrap();
    scan_it(&lib, s.id);
    assert!(!all_files(&lib, s.id).contains("app.log"));

    lib.set_honor_ignore_files(s.id, false).unwrap();
    scan_it(&lib, s.id);
    assert!(all_files(&lib, s.id).contains("app.log"));
}

#[test]
fn user_excludes_and_includes() {
    let (_d, tree, lib) = setup();
    let t = tree.path();
    write(t, "keep/a.txt", "x");
    write(t, "private/b.txt", "x");
    write(t, ".vscode/settings.json", "{}");
    let s = lib.add_source(t, Role::Import, None).unwrap();
    lib.set_source_patterns(s.id, &["private/".into()], &[".vscode".into()])
        .unwrap();
    scan_it(&lib, s.id);
    let got = all_files(&lib, s.id);
    assert!(got.contains("keep/a.txt"));
    assert!(!got.contains("private/b.txt"));
    assert!(got.contains(".vscode/settings.json"));
}

#[test]
fn rescan_tracks_changes_and_removals() {
    let (_d, tree, lib) = setup();
    let t = tree.path();
    write(t, "a.txt", "one");
    write(t, "sub/b.txt", "two");
    write(t, "sub/deeper/c.txt", "three");
    let s = lib.add_source(t, Role::Import, None).unwrap();
    let first = scan_it(&lib, s.id);
    assert_eq!(first.files, 3);

    write(t, "a.txt", "changed and longer");
    fs::remove_dir_all(t.join("sub/deeper")).unwrap();
    write(t, "new.md", "new");
    let second = scan_it(&lib, s.id);
    assert_eq!(second.gone, 1);
    let got = all_files(&lib, s.id);
    assert_eq!(
        got,
        ["a.txt", "new.md", "sub/b.txt"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    );
    let root = lib.source(s.id).unwrap().root_folder_id.unwrap();
    let listing = lib.list(root, SortBy::Name, None, 100).unwrap();
    let a = listing.files.iter().find(|f| f.name == "a.txt").unwrap();
    assert_eq!(a.size, "changed and longer".len() as i64);
    assert_eq!(a.kind, "document");
    let sub = listing.folders.iter().find(|f| f.name == "sub").unwrap();
    assert_eq!(sub.file_count, 1);
    assert_eq!(sub.folder_count, 0);
}

#[test]
fn removable_sources_keep_missing_files() {
    let (_d, tree, lib) = setup();
    let t = tree.path();
    write(t, "a.txt", "one");
    write(t, "b.txt", "two");
    let s = lib.add_source(t, Role::Import, None).unwrap();
    lib.set_source_removable(s.id, true).unwrap();
    scan_it(&lib, s.id);
    fs::remove_file(t.join("b.txt")).unwrap();
    scan_it(&lib, s.id);
    let src = lib.source(s.id).unwrap();
    assert_eq!(src.file_count, 1);
    assert_eq!(src.missing_count, 1);
    // Back again: present once more.
    write(t, "b.txt", "two");
    scan_it(&lib, s.id);
    assert_eq!(lib.source(s.id).unwrap().missing_count, 0);
}

#[test]
fn paging_by_name_size_and_date() {
    let (_d, tree, lib) = setup();
    let t = tree.path();
    for i in 0..25 {
        write(t, &format!("f{i:02}.txt"), &"x".repeat(i + 1));
    }
    let s = lib.add_source(t, Role::Import, None).unwrap();
    scan_it(&lib, s.id);
    let root = lib.source(s.id).unwrap().root_folder_id.unwrap();
    for sort in [SortBy::Name, SortBy::Size, SortBy::Modified] {
        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let page = lib.list(root, sort, after.as_ref(), 10).unwrap();
            seen.extend(page.files.iter().map(|f| f.id));
            match page.next {
                Some(c) => after = Some(c),
                None => break,
            }
        }
        let unique: BTreeSet<_> = seen.iter().collect();
        assert_eq!(seen.len(), 25, "{sort:?}");
        assert_eq!(unique.len(), 25, "{sort:?}");
    }
}

#[test]
fn overlapping_sources_are_refused() {
    let (_d, tree, lib) = setup();
    let t = tree.path();
    write(t, "sub/a.txt", "x");
    lib.add_source(t, Role::Import, None).unwrap();
    assert!(lib.add_source(&t.join("sub"), Role::Import, None).is_err());
    // Adding the same folder again returns the existing source.
    assert_eq!(lib.sources().unwrap().len(), 1);
    lib.add_source(t, Role::Import, None).unwrap();
    assert_eq!(lib.sources().unwrap().len(), 1);
}

#[test]
fn libraries_are_separate() {
    let data = tempfile::tempdir().unwrap();
    let tree = tempfile::tempdir().unwrap();
    write(tree.path(), "a.txt", "x");
    let app = App::open(data.path()).unwrap();
    let personal = app.default_library().unwrap();
    let work = app.create_library("Work").unwrap();
    let p = app.open_library(personal.id).unwrap();
    let w = app.open_library(work.id).unwrap();
    let s = p.add_source(tree.path(), Role::Import, None).unwrap();
    scan_it(&p, s.id);
    assert_eq!(p.status().unwrap().files, 1);
    assert_eq!(w.status().unwrap().files, 0);
    // Close it first: Windows cannot delete an open database file.
    drop(w);
    app.remove_library(work.id).unwrap();
    assert_eq!(app.libraries().unwrap().len(), 1);
    // The user's files are untouched.
    assert!(tree.path().join("a.txt").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_packages_are_single_items() {
    let (_d, tree, lib) = setup();
    let t = tree.path();
    write(t, "Tool.app/Contents/Info.plist", "x");
    write(t, "Tool.app/Contents/MacOS/tool", "x");
    let s = lib.add_source(t, Role::Import, None).unwrap();
    scan_it(&lib, s.id);
    let root = lib.source(s.id).unwrap().root_folder_id.unwrap();
    let l = lib.list(root, SortBy::Name, None, 100).unwrap();
    assert!(l.folders.is_empty());
    assert_eq!(l.files.len(), 1);
    assert_eq!(l.files[0].kind, "package");
}
