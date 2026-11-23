//! Exact duplicates: finding, groups, keeper rules, trash / hard link, blocking, undo.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use docgait_core::{scan, App, DedupeOp, DupeFilter, Library, Role};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn no_progress(_: &docgait_core::ImportProgress) {}

fn setup() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Library,
    PathBuf,
    PathBuf,
) {
    static TRASH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let trash = TRASH.get_or_init(|| {
        std::env::temp_dir().join(format!("docgait-test-trash-{}", std::process::id()))
    });
    std::env::set_var("DOCGAIT_TRASH_DIR", trash);
    let data = tempfile::tempdir().unwrap();
    let dirs = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(dirs.path()).unwrap();
    let library = base.join("Library");
    let drive = base.join("OldDrive");
    fs::create_dir_all(&library).unwrap();
    fs::create_dir_all(&drive).unwrap();
    let app = App::open(data.path()).unwrap();
    let lib = app.open_library(app.default_library().unwrap().id).unwrap();
    lib.set_library_folder(&library).unwrap();
    (data, dirs, lib, library, drive)
}

fn scan_all(lib: &Library) {
    for s in lib.sources().unwrap() {
        scan::scan_source(lib, s.id, &|_| {}, &AtomicBool::new(false)).unwrap();
    }
}

fn find(lib: &Library) -> docgait_core::DupeScan {
    lib.find_duplicates(None, &no_progress, &AtomicBool::new(false))
        .unwrap()
}

#[test]
fn finds_groups_across_sources_and_prefers_the_library_copy() {
    let (_d, _t, lib, library, drive) = setup();
    write(&library, "Taxes/W2.pdf", "w2 contents");
    write(&drive, "backup/W2.pdf", "w2 contents");
    write(&drive, "backup/W2 copy.pdf", "w2 contents");
    write(&drive, "other.txt", "same size!!"); // same size as the W2s, different bytes
    write(&drive, "a/empty.txt", "");
    write(&drive, "b/empty.txt", "");
    lib.add_source(&drive, Role::Import, None).unwrap();
    scan_all(&lib);

    let found = find(&lib);
    assert_eq!(found.groups, 1, "empty files are not duplicates");
    assert_eq!(found.duplicate_files, 2);

    let page = lib.duplicate_groups(&DupeFilter::default(), 0, 50).unwrap();
    assert_eq!(page.total_groups, 1);
    let g = &page.groups[0];
    assert_eq!(g.members.len(), 3);
    assert!(g.in_library);
    let keeper = g.members.iter().find(|m| m.keeper).unwrap();
    assert!(keeper.path.ends_with("W2.pdf") && keeper.path.contains("Library"));
    // The two drive copies are safe to clean up: the library has them.
    assert_eq!(page.safe_files, 2);
    assert_eq!(g.reclaimable, 2 * "w2 contents".len() as i64);

    // Browse badge counts.
    let counts = lib.duplicate_counts(&[keeper.file_id]).unwrap();
    assert_eq!(counts.get(&keeper.file_id), Some(&2));
}

#[test]
fn keeper_rules_prefer_clean_short_names() {
    let (_d, _t, lib, library, _drive) = setup();
    write(&library, "deep/nested/report.doc", "r");
    write(&library, "report (2).doc", "r");
    write(&library, "a/report.doc", "r");
    scan_all(&lib);
    find(&lib);
    let page = lib.duplicate_groups(&DupeFilter::default(), 0, 0).unwrap();
    let keeper = page.groups[0].members.iter().find(|m| m.keeper).unwrap();
    assert!(
        keeper.path.replace('\\', "/").ends_with("a/report.doc"),
        "{}",
        keeper.path
    );
    assert_eq!(
        page.safe_files, 0,
        "copies inside the library are not 'safe'"
    );
}

#[test]
fn trash_blocks_the_last_copy_and_undo_restores() {
    let (_d, _t, lib, library, drive) = setup();
    write(&library, "W2.pdf", "w2");
    write(&drive, "x/W2.pdf", "w2");
    write(&drive, "y/W2.pdf", "w2");
    lib.add_source(&drive, Role::Import, None).unwrap();
    scan_all(&lib);
    find(&lib);

    let all: Vec<i64> = lib
        .duplicate_groups(&DupeFilter::default(), 0, 0)
        .unwrap()
        .groups[0]
        .members
        .iter()
        .map(|m| m.file_id)
        .collect();
    let plan = lib.plan_dedupe(&all, DedupeOp::Trash).unwrap();
    assert_eq!(plan.blocked, 3);
    assert!(lib
        .dedupe(&all, DedupeOp::Trash, &no_progress, &AtomicBool::new(false))
        .is_err());

    let safe = lib
        .auto_select_duplicates(&DupeFilter::default(), true)
        .unwrap();
    assert_eq!(safe.len(), 2);
    let res = lib
        .dedupe(
            &safe,
            DedupeOp::Trash,
            &no_progress,
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(res.done, 2, "{:?}", res.errors);
    assert!(!drive.join("x/W2.pdf").exists() && !drive.join("y/W2.pdf").exists());
    assert!(library.join("W2.pdf").exists());
    assert_eq!(
        lib.duplicate_groups(&DupeFilter::default(), 0, 0)
            .unwrap()
            .total_groups,
        0
    );

    let undo = lib.undo_batch(res.batch_id).unwrap();
    assert_eq!(undo.undone, 2, "{:?}", undo.errors);
    assert!(drive.join("x/W2.pdf").exists() && drive.join("y/W2.pdf").exists());
    // The index has them back (hashes are recomputed on demand).
    find(&lib);
    assert_eq!(
        lib.duplicate_groups(&DupeFilter::default(), 0, 0)
            .unwrap()
            .total_files,
        3
    );
}

#[test]
fn a_changed_file_is_skipped() {
    let (_d, _t, lib, library, drive) = setup();
    write(&library, "a.txt", "aaaa");
    write(&drive, "a.txt", "aaaa");
    lib.add_source(&drive, Role::Import, None).unwrap();
    scan_all(&lib);
    find(&lib);
    let ids = lib
        .auto_select_duplicates(&DupeFilter::default(), false)
        .unwrap();
    assert_eq!(ids.len(), 1);
    // Same size, different bytes, after hashing.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(&drive, "a.txt", "bbbb");
    let res = lib
        .dedupe(&ids, DedupeOp::Trash, &no_progress, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(res.done, 0);
    assert_eq!(res.skipped, 1);
    assert!(drive.join("a.txt").exists());
}

#[cfg(unix)]
#[test]
fn hardlink_keeps_the_path_and_undo_separates_again() {
    let (_d, _t, lib, library, _drive) = setup();
    write(&library, "a/big.bin", "same bytes");
    write(&library, "b/big copy.bin", "same bytes");
    scan_all(&lib);
    find(&lib);
    let ids = lib
        .auto_select_duplicates(&DupeFilter::default(), false)
        .unwrap();
    assert_eq!(ids.len(), 1);
    let res = lib
        .dedupe(
            &ids,
            DedupeOp::Hardlink,
            &no_progress,
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(res.done, 1, "{:?}", res.errors);
    use std::os::unix::fs::MetadataExt;
    let ino = |p: &Path| fs::metadata(p).unwrap().ino();
    let (a, b) = (library.join("a/big.bin"), library.join("b/big copy.bin"));
    assert_eq!(ino(&a), ino(&b));
    let page = lib.duplicate_groups(&DupeFilter::default(), 0, 0).unwrap();
    assert_eq!(page.total_reclaimable, 0, "hard links share their bytes");
    assert!(page.groups[0].members.iter().all(|m| m.hardlinked));
    // Linking again is a no-op.
    let again = lib.plan_dedupe(&ids, DedupeOp::Hardlink).unwrap();
    assert_eq!(again.ok, 0);

    lib.undo_batch(res.batch_id).unwrap();
    assert_ne!(ino(&a), ino(&b));
    assert_eq!(fs::read_to_string(&b).unwrap(), "same bytes");
}

#[test]
fn ignored_groups_are_hidden() {
    let (_d, _t, lib, library, _drive) = setup();
    write(&library, "x/LICENSE", "MIT");
    write(&library, "y/LICENSE", "MIT");
    scan_all(&lib);
    find(&lib);
    let g = lib
        .duplicate_groups(&DupeFilter::default(), 0, 0)
        .unwrap()
        .groups[0]
        .clone();
    lib.ignore_duplicate_group(&g.hash, true).unwrap();
    assert_eq!(
        lib.duplicate_groups(&DupeFilter::default(), 0, 0)
            .unwrap()
            .total_groups,
        0
    );
    let with = DupeFilter {
        include_ignored: true,
        ..Default::default()
    };
    assert!(lib.duplicate_groups(&with, 0, 0).unwrap().groups[0].ignored);
    lib.ignore_duplicate_group(&g.hash, false).unwrap();
    assert_eq!(
        lib.duplicate_groups(&DupeFilter::default(), 0, 0)
            .unwrap()
            .total_groups,
        1
    );
}

#[test]
fn folder_scope_only_hashes_what_could_match() {
    let (_d, _t, lib, library, drive) = setup();
    write(&library, "keep/a.txt", "one");
    write(&drive, "in/a.txt", "one");
    write(&drive, "elsewhere/p.txt", "two two");
    write(&drive, "elsewhere/q.txt", "two two");
    let sid = lib.add_source(&drive, Role::Import, None).unwrap().id;
    scan_all(&lib);
    let folder = lib
        .list(
            lib.source(sid).unwrap().root_folder_id.unwrap(),
            Default::default(),
            None,
            100,
        )
        .unwrap()
        .folders
        .into_iter()
        .find(|f| f.name == "in")
        .unwrap();
    lib.find_duplicates(Some(folder.id), &no_progress, &AtomicBool::new(false))
        .unwrap();
    let page = lib.duplicate_groups(&DupeFilter::default(), 0, 0).unwrap();
    assert_eq!(page.total_groups, 1, "p/q were never compared");
}
