//! Conflicts after an import: list, accept suggestions, keep one, keep all, merge, undo.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use docgait_core::{scan, App, ConflictView, Layout, Library, Resolution, Role};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn age(root: &Path, rel: &str, days: u64, secs: u64) {
    let t = SystemTime::now() - Duration::from_secs(days * 86_400 + secs);
    fs::File::options()
        .write(true)
        .open(root.join(rel))
        .unwrap()
        .set_modified(t)
        .unwrap();
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    v.sort();
    v
}

fn no_progress(_: &docgait_core::ImportProgress) {}

/// Library after importing two copies: Notes/plan.md (clear newest) and Letters/note.txt
/// (dates one second apart: needs review).
fn setup() -> (tempfile::TempDir, tempfile::TempDir, Library, PathBuf) {
    static TRASH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let trash = TRASH.get_or_init(|| {
        std::env::temp_dir().join(format!("docgait-test-trash-{}", std::process::id()))
    });
    std::env::set_var("DOCGAIT_TRASH_DIR", trash);
    let data = tempfile::tempdir().unwrap();
    let dirs = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(dirs.path()).unwrap();
    let library = base.join("Library");
    let (a, b) = (base.join("CopyA"), base.join("CopyB"));
    fs::create_dir_all(&library).unwrap();
    write(&a, "Notes/plan.md", "intro\nstep one\noutro\n");
    write(&b, "Notes/plan.md", "intro\nstep one, revised\noutro\n");
    age(&a, "Notes/plan.md", 20, 0);
    age(&b, "Notes/plan.md", 2, 0);
    write(&a, "Letters/note.txt", "dear a\n");
    write(&b, "Letters/note.txt", "dear b\n");
    age(&a, "Letters/note.txt", 5, 1);
    age(&b, "Letters/note.txt", 5, 0);
    let app = App::open(data.path()).unwrap();
    let lib = app.open_library(app.default_library().unwrap().id).unwrap();
    lib.set_library_folder(&library).unwrap();
    let mut roots = Vec::new();
    for c in [&a, &b] {
        let s = lib.add_source(c, Role::Import, None).unwrap();
        scan::scan_source(&lib, s.id, &|_| {}, &AtomicBool::new(false)).unwrap();
        roots.push(lib.source(s.id).unwrap().root_folder_id.unwrap());
    }
    let cancel = AtomicBool::new(false);
    let plan = lib
        .plan_import(&roots, "", Layout::Merge, &no_progress, &cancel)
        .unwrap();
    lib.execute_import(plan.run_id, &no_progress, &cancel)
        .unwrap();
    (data, dirs, lib, library)
}

fn find<'a>(cs: &'a [ConflictView], name: &str) -> &'a ConflictView {
    cs.iter().find(|c| c.name == name).unwrap()
}

#[test]
fn lists_and_accepts_suggestions_with_undo() {
    let (_d, _t, lib, library) = setup();
    let cs = lib.conflicts(false).unwrap();
    assert_eq!(cs.len(), 2);
    assert_eq!(find(&cs, "plan.md").state, "suggested");
    assert_eq!(find(&cs, "note.txt").state, "needs_review");
    assert_eq!(find(&cs, "plan.md").folder, "Notes");
    assert!(find(&cs, "plan.md")
        .variants
        .iter()
        .all(|v| v.text && v.exists));
    assert_eq!(lib.open_conflict_count().unwrap(), 2);

    let (batch, n) = lib.accept_suggestions().unwrap();
    assert_eq!(n, 1);
    assert_eq!(names(&library.join("Notes")), vec!["plan.md"]);
    assert_eq!(lib.open_conflict_count().unwrap(), 1);

    lib.undo_batch(batch).unwrap();
    assert_eq!(names(&library.join("Notes")).len(), 2);
    assert_eq!(lib.open_conflict_count().unwrap(), 2);
    assert_eq!(
        find(&lib.conflicts(false).unwrap(), "plan.md").state,
        "suggested"
    );
}

#[test]
fn keep_another_version() {
    let (_d, _t, lib, library) = setup();
    let cs = lib.conflicts(false).unwrap();
    let note = find(&cs, "note.txt");
    let other = note.variants.iter().find(|v| !v.is_current).unwrap();
    let other_text = fs::read_to_string(&other.path).unwrap();
    let batch = lib
        .resolve_conflict(
            note.id,
            &Resolution::KeepVariant {
                path: other.path.clone(),
            },
        )
        .unwrap();
    assert_eq!(names(&library.join("Letters")), vec!["note.txt"]);
    assert_eq!(
        fs::read_to_string(library.join("Letters/note.txt")).unwrap(),
        other_text
    );
    // Undo puts both back exactly as they were.
    let before_current = note.variants.iter().find(|v| v.is_current).unwrap();
    lib.undo_batch(batch).unwrap();
    assert_eq!(names(&library.join("Letters")).len(), 2);
    assert_eq!(fs::read_to_string(&other.path).unwrap(), other_text);
    assert_ne!(
        fs::read_to_string(&before_current.path).unwrap(),
        other_text
    );
}

#[test]
fn merge_text_and_keep_all() {
    let (_d, _t, lib, library) = setup();
    let cs = lib.conflicts(false).unwrap();
    let plan = find(&cs, "plan.md");
    let other = plan.variants.iter().find(|v| !v.is_current).unwrap();
    let m = lib.merge_start(plan.id, &other.path).unwrap();
    assert!(!m.clean);
    assert!(m.result.starts_with("intro\n<<<<<<< current"));
    assert!(
        m.result
            .contains("=======\nstep one\n>>>>>>> CopyA\noutro\n"),
        "{}",
        m.result
    );
    // Unresolved blocks are refused.
    assert!(lib
        .resolve_conflict(
            plan.id,
            &Resolution::Merge {
                text: m.result.clone()
            }
        )
        .is_err());
    let merged = "intro\nstep one, revised (merged)\noutro\n".to_string();
    let batch = lib
        .resolve_conflict(
            plan.id,
            &Resolution::Merge {
                text: merged.clone(),
            },
        )
        .unwrap();
    assert_eq!(names(&library.join("Notes")), vec!["plan.md"]);
    assert_eq!(
        fs::read_to_string(library.join("Notes/plan.md")).unwrap(),
        merged
    );
    lib.undo_batch(batch).unwrap();
    assert_eq!(names(&library.join("Notes")).len(), 2);
    assert_eq!(
        fs::read_to_string(library.join("Notes/plan.md")).unwrap(),
        "intro\nstep one, revised\noutro\n"
    );

    // Keep all: the conflict file gets a normal name.
    let note = find(&lib.conflicts(false).unwrap(), "note.txt").clone();
    let other = note.variants.iter().find(|v| !v.is_current).unwrap();
    lib.resolve_conflict(
        note.id,
        &Resolution::KeepAll {
            names: vec![(other.path.clone(), "note (from B).txt".into())],
        },
    )
    .unwrap();
    assert_eq!(
        names(&library.join("Letters")),
        vec!["note (from B).txt", "note.txt"]
    );
    assert!(lib
        .conflicts(false)
        .unwrap()
        .iter()
        .all(|c| c.name != "note.txt"));
}

#[test]
fn finds_conflict_files_it_did_not_create() {
    let (_d, _t, lib, library) = setup();
    write(&library, "Misc/x.txt", "one");
    write(&library, "Misc/x.conflict-OldPC-2020-01-01.txt", "two");
    let src = lib.library_source().unwrap().unwrap();
    scan::scan_source(&lib, src.id, &|_| {}, &AtomicBool::new(false)).unwrap();
    let cs = lib.conflicts(false).unwrap();
    let x = find(&cs, "x.txt");
    assert_eq!(x.state, "needs_review");
    assert_eq!(x.variants.len(), 2);
    assert!(x.variants.iter().any(|v| v.origin == "OldPC"));
    lib.resolve_conflict(x.id, &Resolution::KeepCurrent)
        .unwrap();
    assert_eq!(names(&library.join("Misc")), vec!["x.txt"]);
}
