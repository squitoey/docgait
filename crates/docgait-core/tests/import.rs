//! Import into the library: the M2 exit scenario with three overlapping copies of a tree.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use docgait_core::{scan, App, Layout, Library, Role};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// Sets the modified time to `days` days ago (plus `secs`).
fn age(root: &Path, rel: &str, days: u64, secs: u64) {
    let t = SystemTime::now() - Duration::from_secs(days * 86_400 + secs);
    let f = fs::File::options()
        .write(true)
        .open(root.join(rel))
        .unwrap();
    f.set_modified(t).unwrap();
}

/// Every file under `root` (relative, '/'-separated), skipping nothing.
fn tree(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    fn walk(base: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                let rel = p.strip_prefix(base).unwrap();
                out.insert(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    walk(root, root, &mut out);
    out
}

fn no_progress(_: &docgait_core::ImportProgress) {}

struct Fixture {
    _data: tempfile::TempDir,
    _dirs: tempfile::TempDir,
    lib: Library,
    library: std::path::PathBuf,
    copies: Vec<std::path::PathBuf>,
}

/// Three copies of the same data, each with its own changes, and an empty library folder.
fn fixture() -> Fixture {
    // Keep test files out of the real trash.
    static TRASH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    let trash = TRASH.get_or_init(|| {
        std::env::temp_dir().join(format!("docgait-test-trash-{}", std::process::id()))
    });
    std::env::set_var("DOCGAIT_TRASH_DIR", trash);
    let data = tempfile::tempdir().unwrap();
    let dirs = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(dirs.path()).unwrap();
    let library = base.join("Library");
    fs::create_dir_all(&library).unwrap();
    let copies: Vec<_> = ["CopyA", "CopyB", "CopyC"]
        .iter()
        .map(|n| base.join(n))
        .collect();
    for c in &copies {
        write(c, "Taxes/2023/W2.pdf", "w2 2023");
        write(c, "Photos/beach.jpg", "beach");
        age(c, "Taxes/2023/W2.pdf", 400, 0);
    }
    // Unique to each copy.
    write(&copies[0], "Notes/only-a.txt", "a");
    write(&copies[1], "Notes/only-b.txt", "b");
    write(&copies[2], "Recipes/pie.md", "pie");
    // The same path edited differently: B's edit is the newest (days apart, so dates are trusted).
    write(&copies[0], "Notes/plan.md", "plan v1");
    write(&copies[1], "Notes/plan.md", "plan v2 (newest)");
    write(&copies[2], "Notes/plan.md", "plan v1.5");
    age(&copies[0], "Notes/plan.md", 30, 0);
    age(&copies[1], "Notes/plan.md", 2, 0);
    age(&copies[2], "Notes/plan.md", 10, 0);
    // A duplicate under another name within one copy: comes in once, with the clean name.
    write(&copies[0], "Photos/beach (2).jpg", "beach");
    // Same path, dates that cannot be trusted (one second apart): needs review.
    write(&copies[0], "Letters/note.txt", "version A");
    write(&copies[1], "Letters/note.txt", "version B");
    age(&copies[0], "Letters/note.txt", 5, 1);
    age(&copies[1], "Letters/note.txt", 5, 0);

    let app = App::open(data.path()).unwrap();
    let lib = app.open_library(app.default_library().unwrap().id).unwrap();
    lib.set_library_folder(&library).unwrap();
    for c in &copies {
        let s = lib.add_source(c, Role::Import, None).unwrap();
        scan::scan_source(&lib, s.id, &|_| {}, &AtomicBool::new(false)).unwrap();
    }
    Fixture {
        _data: data,
        _dirs: dirs,
        lib,
        library,
        copies,
    }
}

fn root_folders(lib: &Library) -> Vec<i64> {
    lib.sources()
        .unwrap()
        .into_iter()
        .filter(|s| s.role == Role::Import)
        .map(|s| s.root_folder_id.unwrap())
        .collect()
}

#[test]
fn merges_three_copies_into_one_library() {
    let f = fixture();
    let cancel = AtomicBool::new(false);
    let plan = f
        .lib
        .plan_import(
            &root_folders(&f.lib),
            "",
            Layout::Merge,
            &no_progress,
            &cancel,
        )
        .unwrap();
    // Nothing on disk yet.
    assert!(tree(&f.library).is_empty());
    assert_eq!(plan.conflicts, 2);
    assert_eq!(plan.needs_review, 1);

    let res = f
        .lib
        .execute_import(plan.run_id, &no_progress, &cancel)
        .unwrap();
    assert_eq!(res.failed, 0, "{:?}", res.errors);

    let got = tree(&f.library);
    // Every unique file is there exactly once.
    for want in [
        "Taxes/2023/W2.pdf",
        "Photos/beach.jpg",
        "Notes/only-a.txt",
        "Notes/only-b.txt",
        "Recipes/pie.md",
        "Notes/plan.md",
        "Letters/note.txt",
    ] {
        assert!(got.contains(want), "missing {want}: {got:?}");
    }
    assert!(
        !got.contains("Photos/beach (2).jpg"),
        "duplicate imported: {got:?}"
    );
    // The newest plan.md keeps the name; the others sit next to it as conflict files.
    assert_eq!(
        fs::read_to_string(f.library.join("Notes/plan.md")).unwrap(),
        "plan v2 (newest)"
    );
    let plan_conflicts: Vec<_> = got
        .iter()
        .filter(|p| p.starts_with("Notes/plan.conflict-"))
        .collect();
    assert_eq!(plan_conflicts.len(), 2, "{got:?}");
    assert!(plan_conflicts.iter().any(|p| p.contains("conflict-CopyA-")));
    assert!(plan_conflicts.iter().any(|p| p.contains("conflict-CopyC-")));
    assert!(plan_conflicts.iter().all(|p| p.ends_with(".md")));
    // Unreliable dates: both kept, one keeps the name, one is a conflict file.
    let notes: Vec<_> = got
        .iter()
        .filter(|p| p.starts_with("Letters/note"))
        .collect();
    assert_eq!(notes.len(), 2, "{got:?}");
    // Total: 7 real files + 2 plan conflicts + 1 note conflict.
    assert_eq!(got.len(), 10, "{got:?}");
    // Every distinct content from the copies is in the library.
    let contents: BTreeSet<String> = got
        .iter()
        .map(|p| fs::read_to_string(f.library.join(p)).unwrap())
        .collect();
    assert_eq!(contents.len(), 10);
    // Sources untouched.
    assert!(f.copies[0].join("Photos/beach (2).jpg").exists());
    assert!(f.copies[1].join("Notes/plan.md").exists());

    // Importing again changes nothing: everything is already in the library.
    let again = f
        .lib
        .plan_import(
            &root_folders(&f.lib),
            "",
            Layout::Merge,
            &no_progress,
            &cancel,
        )
        .unwrap();
    assert_eq!(again.to_copy, 0, "{again:?}");
    assert_eq!(again.conflicts, 0);

    // Undo removes everything the import added.
    let undo = f.lib.undo_batch(res.batch_id).unwrap();
    assert_eq!(undo.failed, 0, "{:?}", undo.errors);
    assert!(tree(&f.library).is_empty(), "{:?}", tree(&f.library));
    let s = f.lib.library_source().unwrap().unwrap();
    assert_eq!(s.file_count, 0);
}

#[test]
fn existing_library_file_makes_room_for_a_newer_version() {
    let f = fixture();
    // The library already has an older plan.md.
    write(&f.library, "Notes/plan.md", "plan v0 (library)");
    age(&f.library, "Notes/plan.md", 100, 0);
    let lib_src = f.lib.library_source().unwrap().unwrap();
    scan::scan_source(&f.lib, lib_src.id, &|_| {}, &AtomicBool::new(false)).unwrap();

    let cancel = AtomicBool::new(false);
    let b = f
        .lib
        .sources()
        .unwrap()
        .into_iter()
        .find(|s| s.root.ends_with("CopyB"))
        .unwrap();
    let plan = f
        .lib
        .plan_import(
            &[b.root_folder_id.unwrap()],
            "",
            Layout::Merge,
            &no_progress,
            &cancel,
        )
        .unwrap();
    let res = f
        .lib
        .execute_import(plan.run_id, &no_progress, &cancel)
        .unwrap();
    assert_eq!(res.failed, 0, "{:?}", res.errors);
    assert_eq!(res.renamed, 1);
    assert_eq!(
        fs::read_to_string(f.library.join("Notes/plan.md")).unwrap(),
        "plan v2 (newest)"
    );
    let got = tree(&f.library);
    let old: Vec<_> = got
        .iter()
        .filter(|p| p.contains("plan.conflict-library-"))
        .collect();
    assert_eq!(old.len(), 1, "{got:?}");
    assert_eq!(
        fs::read_to_string(f.library.join(old[0])).unwrap(),
        "plan v0 (library)"
    );

    // Undo puts the library file back under its own name.
    f.lib.undo_batch(res.batch_id).unwrap();
    assert_eq!(
        fs::read_to_string(f.library.join("Notes/plan.md")).unwrap(),
        "plan v0 (library)"
    );
    assert_eq!(tree(&f.library).len(), 1);
}

#[test]
fn own_folder_layout_and_destination() {
    let f = fixture();
    let cancel = AtomicBool::new(false);
    let a = f
        .lib
        .sources()
        .unwrap()
        .into_iter()
        .find(|s| s.root.ends_with("CopyA"))
        .unwrap();
    let plan = f
        .lib
        .plan_import(
            &[a.root_folder_id.unwrap()],
            "Imported",
            Layout::OwnFolder,
            &no_progress,
            &cancel,
        )
        .unwrap();
    f.lib
        .execute_import(plan.run_id, &no_progress, &cancel)
        .unwrap();
    let got = tree(&f.library);
    assert!(got.contains("Imported/CopyA/Taxes/2023/W2.pdf"), "{got:?}");
    assert!(got.iter().all(|p| p.starts_with("Imported/CopyA/")));
}

#[test]
fn refuses_unsafe_imports() {
    let f = fixture();
    let cancel = AtomicBool::new(false);
    // The library itself.
    let lib_root = f
        .lib
        .library_source()
        .unwrap()
        .unwrap()
        .root_folder_id
        .unwrap();
    assert!(f
        .lib
        .plan_import(&[lib_root], "", Layout::Merge, &no_progress, &cancel)
        .is_err());
    // A file changed after planning is not copied.
    let a = f
        .lib
        .sources()
        .unwrap()
        .into_iter()
        .find(|s| s.root.ends_with("CopyA"))
        .unwrap();
    let plan = f
        .lib
        .plan_import(
            &[a.root_folder_id.unwrap()],
            "",
            Layout::Merge,
            &no_progress,
            &cancel,
        )
        .unwrap();
    write(&f.copies[0], "Notes/only-a.txt", "changed after planning");
    let res = f
        .lib
        .execute_import(plan.run_id, &no_progress, &cancel)
        .unwrap();
    assert_eq!(res.failed, 1);
    assert!(!f.library.join("Notes/only-a.txt").exists());
    // Re-running the same plan only retries what is left (and fails it again, safely).
    let again = f
        .lib
        .execute_import(plan.run_id, &no_progress, &cancel)
        .unwrap();
    assert_eq!(again.copied, 0);
}

#[test]
fn vault_and_repo_folders_travel_with_their_content() {
    let f = fixture();
    let cancel = AtomicBool::new(false);
    // A vault in A and C (different settings), a repository in B. All excluded from the index.
    write(&f.copies[0], "Vault/.obsidian/app.json", "{\"from\":\"A\"}");
    write(&f.copies[0], "Vault/Home.md", "see [[Ideas]]");
    write(&f.copies[2], "Vault/.obsidian/app.json", "{\"from\":\"C\"}");
    write(&f.copies[2], "Vault/Ideas.md", "ideas");
    write(&f.copies[1], "Code/tool/.git/HEAD", "ref: refs/heads/main");
    write(&f.copies[1], "Code/tool/.git/objects/ab/cdef", "blob");
    write(&f.copies[1], "Code/tool/main.rs", "fn main() {}");
    // At the root of a copy: never makes the whole library a repository.
    write(&f.copies[1], ".git/HEAD", "ref: refs/heads/root");
    for s in f.lib.sources().unwrap() {
        scan::scan_source(&f.lib, s.id, &|_| {}, &cancel).unwrap();
    }
    let plan = f
        .lib
        .plan_import(
            &root_folders(&f.lib),
            "",
            Layout::Merge,
            &no_progress,
            &cancel,
        )
        .unwrap();
    let res = f
        .lib
        .execute_import(plan.run_id, &no_progress, &cancel)
        .unwrap();
    assert_eq!(res.failed, 0, "{:?}", res.errors);
    let got = tree(&f.library);
    assert!(got.contains("Vault/.obsidian/app.json"), "{got:?}");
    assert!(got.contains("Code/tool/.git/HEAD") && got.contains("Code/tool/.git/objects/ab/cdef"));
    assert!(
        !got.contains(".git/HEAD"),
        "library root must not become a repo"
    );
    // One vault's settings, never merged with the other's.
    let app = fs::read_to_string(f.library.join("Vault/.obsidian/app.json")).unwrap();
    assert!(app.contains("\"A\"") || app.contains("\"C\""));
    assert!(!got.iter().any(|p| p.contains(".obsidian/app.conflict")));

    // Undo takes the carried folders away too.
    f.lib.undo_batch(res.batch_id).unwrap();
    assert!(!f.library.join("Vault/.obsidian/app.json").exists());
    assert!(!f.library.join("Code/tool/.git/HEAD").exists());
}
