//! Clean-up after import, coverage, and the Excluded summary.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use docgait_core::{scan, App, Layout, Library, Role};

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
    let copy = base.join("OldDrive");
    fs::create_dir_all(&library).unwrap();
    let app = App::open(data.path()).unwrap();
    let lib = app.open_library(app.default_library().unwrap().id).unwrap();
    lib.set_library_folder(&library).unwrap();
    (data, dirs, lib, library, copy)
}

fn add_and_scan(lib: &Library, path: &Path) -> i64 {
    let s = lib.add_source(path, Role::Import, None).unwrap();
    scan::scan_source(lib, s.id, &|_| {}, &AtomicBool::new(false)).unwrap();
    s.id
}

fn files_in(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(
                    p.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    if root.exists() {
        walk(root, root, &mut out);
    }
    out.sort();
    out
}

#[test]
fn cleanup_trashes_only_what_the_library_has_and_undo_restores() {
    let (_d, _t, lib, library, copy) = setup();
    write(&copy, "Taxes/2023/W2.pdf", "w2");
    write(&copy, "Photos/beach.jpg", "beach");
    write(&copy, "Photos/sunset.jpg", "sunset");
    write(&copy, "Empty-after/only.txt", "only");
    let sid = add_and_scan(&lib, &copy);
    let root = lib.source(sid).unwrap().root_folder_id.unwrap();
    let cancel = AtomicBool::new(false);

    // Import everything, then add a file to the drive that the library does not have.
    let plan = lib
        .plan_import(&[root], "", Layout::Merge, &no_progress, &cancel)
        .unwrap();
    lib.execute_import(plan.run_id, &no_progress, &cancel)
        .unwrap();
    write(&copy, "Photos/new-since.jpg", "new");
    scan::scan_source(&lib, sid, &|_| {}, &AtomicBool::new(false)).unwrap();

    let cov = lib.check_coverage(root, &no_progress, &cancel).unwrap();
    assert_eq!((cov.files, cov.in_library, cov.not_in_library), (5, 4, 1));
    assert_eq!(lib.coverage(sid).unwrap().unwrap().in_library, 4);
    let missing = lib
        .coverage_files(root, false, 10, &no_progress, &cancel)
        .unwrap();
    assert_eq!(missing.len(), 1);
    assert!(missing[0].path.ends_with("new-since.jpg"));

    let res = lib.cleanup(root, &no_progress, &cancel).unwrap();
    assert_eq!(res.trashed, 4, "{:?}", res.errors);
    assert_eq!(res.skipped, 0);
    // Only the file the library does not have is left; emptied folders are gone.
    assert_eq!(files_in(&copy), vec!["Photos/new-since.jpg"]);
    assert!(!copy.join("Taxes").exists());
    assert!(!copy.join("Empty-after").exists());
    // The library is untouched.
    assert_eq!(files_in(&library).len(), 4);
    assert_eq!(lib.source(sid).unwrap().file_count, 1);

    // Undo brings every file back where it was.
    let undo = lib.undo_batch(res.batch_id).unwrap();
    assert_eq!(undo.failed, 0, "{:?}", undo.errors);
    assert_eq!(
        files_in(&copy),
        vec![
            "Empty-after/only.txt",
            "Photos/beach.jpg",
            "Photos/new-since.jpg",
            "Photos/sunset.jpg",
            "Taxes/2023/W2.pdf"
        ]
    );
    assert_eq!(lib.source(sid).unwrap().file_count, 5);
}

#[test]
fn cleanup_skips_files_that_changed() {
    let (_d, _t, lib, library, copy) = setup();
    write(&copy, "a.txt", "same");
    write(&copy, "b.txt", "same-b");
    let sid = add_and_scan(&lib, &copy);
    let root = lib.source(sid).unwrap().root_folder_id.unwrap();
    let cancel = AtomicBool::new(false);
    let plan = lib
        .plan_import(&[root], "", Layout::Merge, &no_progress, &cancel)
        .unwrap();
    lib.execute_import(plan.run_id, &no_progress, &cancel)
        .unwrap();
    // The library copy of b.txt is edited: the drive's b.txt is no longer in the library.
    write(&library, "b.txt", "edited in the library");
    let lib_src = lib.library_source().unwrap().unwrap();
    scan::scan_source(&lib, lib_src.id, &|_| {}, &AtomicBool::new(false)).unwrap();
    let res = lib.cleanup(root, &no_progress, &cancel).unwrap();
    assert_eq!(res.trashed, 1);
    assert!(copy.join("b.txt").exists());
    assert!(!copy.join("a.txt").exists());
}

#[test]
fn excluded_summary_and_index_anyway() {
    let (_d, _t, lib, _library, copy) = setup();
    write(&copy, "keep.txt", "x");
    write(&copy, ".gitignore", "secret.env\n");
    write(&copy, "secret.env", "x");
    write(&copy, "app/node_modules/x/index.js", "x");
    write(&copy, "private/notes.txt", "x");
    write(&copy, ".DS_Store", "x");
    write(
        &copy,
        "cache/CACHEDIR.TAG",
        "Signature: 8a477f597d28d172789f06886806bc55",
    );
    #[cfg(unix)]
    std::os::unix::fs::symlink(copy.join("keep.txt"), copy.join("link.txt")).unwrap();
    let sid = add_and_scan(&lib, &copy);
    lib.set_source_patterns(sid, &["private/".into()], &[])
        .unwrap();

    let ex = lib.excluded(sid).unwrap();
    let reasons: BTreeMap<String, &str> = ex
        .entries
        .iter()
        .map(|e| (e.rel.clone(), e.reason))
        .collect();
    assert_eq!(
        reasons.get("secret.env"),
        Some(&"ignore-file"),
        "{reasons:?}"
    );
    assert_eq!(reasons.get("app/node_modules"), Some(&"builtin"));
    assert_eq!(reasons.get(".DS_Store"), Some(&"builtin"));
    assert_eq!(reasons.get("private"), Some(&"pattern"));
    assert_eq!(reasons.get("cache"), Some(&"cachedir"));
    #[cfg(unix)]
    assert_eq!(reasons.get("link.txt"), Some(&"symlink"));
    assert!(!reasons.contains_key("keep.txt"));
    // Nothing inside an excluded folder is listed separately.
    assert!(!reasons.keys().any(|k| k.starts_with("app/node_modules/")));

    // Bring back the ignored file (via .docgaitignore) and the pattern-excluded folder.
    lib.include_anyway(sid, "secret.env", "ignore-file")
        .unwrap();
    lib.include_anyway(sid, "private", "pattern").unwrap();
    assert!(fs::read_to_string(copy.join(".docgaitignore"))
        .unwrap()
        .contains("!/secret.env"));
    let ex = lib.excluded(sid).unwrap();
    assert!(!ex
        .entries
        .iter()
        .any(|e| e.rel == "secret.env" || e.rel == "private"));
    // A built-in exclude comes back through the source's includes.
    lib.include_anyway(sid, "app/node_modules", "builtin")
        .unwrap();
    assert!(lib
        .source(sid)
        .unwrap()
        .includes
        .contains(&"node_modules".to_string()));
    assert!(!lib
        .excluded(sid)
        .unwrap()
        .entries
        .iter()
        .any(|e| e.rel == "app/node_modules"));
}
