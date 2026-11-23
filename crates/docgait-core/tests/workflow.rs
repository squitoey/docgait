//! The whole workflow, end to end, the way a person uses it: three overlapping copies of a
//! tree (with a vault, a repo, junk and duplicates) → one library → conflicts resolved →
//! sources cleaned up → leftover duplicates removed → organised → everything undone.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use docgait_core::{scan, App, DedupeOp, DupeFilter, Layout, Library, Resolution, Role};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn age(root: &Path, rel: &str, days: u64) {
    let t = SystemTime::now() - Duration::from_secs(days * 86_400);
    fs::File::options()
        .write(true)
        .open(root.join(rel))
        .unwrap()
        .set_modified(t)
        .unwrap();
}

fn tree(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    fn walk(base: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.insert(
                    p.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    walk(root, root, &mut out);
    out
}

fn np(_: &docgait_core::ImportProgress) {}

fn rescan(lib: &Library) {
    for s in lib.sources().unwrap() {
        if s.online {
            scan::scan_source(lib, s.id, &|_| {}, &AtomicBool::new(false)).unwrap();
        }
    }
}

#[test]
fn three_copies_to_one_clean_library_and_back() {
    static TRASH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let trash = TRASH.get_or_init(|| {
        std::env::temp_dir().join(format!("docgait-test-trash-{}", std::process::id()))
    });
    std::env::set_var("DOCGAIT_TRASH_DIR", trash);
    let data = tempfile::tempdir().unwrap();
    let dirs = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(dirs.path()).unwrap();
    let library = base.join("Library");
    fs::create_dir_all(&library).unwrap();
    let copies: Vec<PathBuf> = ["CopyA", "CopyB", "CopyC"]
        .iter()
        .map(|n| base.join(n))
        .collect();
    for c in &copies {
        write(c, "Taxes/2023/W2.pdf", "w2 2023");
        write(c, "Projects/app/.gitignore", "build/\n");
        write(c, "Projects/app/src/main.rs", "fn main() {}");
        write(c, "Projects/app/build/out.bin", "junk junk");
        write(c, "Projects/app/node_modules/x/index.js", "x");
    }
    write(&copies[0], "only-a.txt", "unique to a");
    write(&copies[1], "Taxes/receipt.pdf", "receipt");
    write(&copies[1], "Taxes/receipt (2).pdf", "receipt");
    write(&copies[0], "Notes/plan.md", "# Plan\none\ntwo\n");
    write(
        &copies[1],
        "Notes/plan.md",
        "# Plan\none\n2 (changed)\nthree\n",
    );
    age(&copies[0], "Notes/plan.md", 40);
    age(&copies[1], "Notes/plan.md", 3);
    write(&copies[2], "Vault/.obsidian/app.json", "{}");
    write(&copies[2], "Vault/Home.md", "See [[Ideas]]");
    write(&copies[2], "Vault/Ideas.md", "ideas");

    let app = App::open(data.path()).unwrap();
    let lib = app.open_library(app.default_library().unwrap().id).unwrap();
    lib.set_library_folder(&library).unwrap();
    for c in &copies {
        lib.add_source(c, Role::Import, None).unwrap();
    }
    rescan(&lib);
    let cancel = AtomicBool::new(false);
    let roots: Vec<i64> = lib
        .sources()
        .unwrap()
        .iter()
        .filter(|s| s.role == Role::Import)
        .map(|s| s.root_folder_id.unwrap())
        .collect();

    // 1. Import: one copy of everything, junk left behind, the vault stays a vault.
    let plan = lib
        .plan_import(&roots, "", Layout::Merge, &np, &cancel)
        .unwrap();
    assert_eq!(plan.conflicts, 1);
    let imported = lib.execute_import(plan.run_id, &np, &cancel).unwrap();
    assert_eq!(imported.failed, 0, "{:?}", imported.errors);
    let got = tree(&library);
    for want in [
        "Taxes/2023/W2.pdf",
        "Taxes/receipt.pdf",
        "only-a.txt",
        "Projects/app/src/main.rs",
        "Projects/app/.gitignore",
        "Notes/plan.md",
        "Vault/Home.md",
        "Vault/.obsidian/app.json",
    ] {
        assert!(got.contains(want), "missing {want}: {got:?}");
    }
    assert!(!got
        .iter()
        .any(|p| p.contains("node_modules") || p.contains("build/")));
    assert!(
        !got.contains("Taxes/receipt (2).pdf"),
        "duplicate came in once"
    );
    assert_eq!(
        fs::read_to_string(library.join("Notes/plan.md")).unwrap(),
        "# Plan\none\n2 (changed)\nthree\n",
        "the newest version keeps the name"
    );

    // 2. Resolve the conflict by keeping the suggested (current) version.
    let conflicts = lib.conflicts(false).unwrap();
    assert_eq!(conflicts.len(), 1);
    lib.resolve_conflict(conflicts[0].id, &Resolution::KeepCurrent)
        .unwrap();
    assert_eq!(lib.open_conflict_count().unwrap(), 0);
    assert!(!tree(&library).iter().any(|p| p.contains(".conflict-")));

    // 3. Clean up copy A: everything it has is in the library. The .gitignore stays because
    //    build/ and node_modules/ are still there (otherwise a rescan would index the junk).
    let a_root = roots[0];
    let cov = lib.check_coverage(a_root, &np, &cancel).unwrap();
    // A's plan.md was the version dropped when the conflict was resolved: not in the library.
    assert_eq!(cov.not_in_library, 1);
    let cleaned = lib.cleanup(a_root, &np, &cancel).unwrap();
    assert_eq!(cleaned.kept_ignore_files, 1);
    assert!(copies[0].join("Projects/app/.gitignore").exists());
    assert!(!copies[0].join("Taxes/2023/W2.pdf").exists());
    assert!(
        copies[0].join("Notes/plan.md").exists(),
        "never trash what the library lacks"
    );
    rescan(&lib);
    assert!(
        lib.search("out.bin", 10).unwrap().hits.is_empty(),
        "junk must stay excluded after clean-up"
    );

    // 4. Remaining duplicates on B and C are all "safe" (the library has them).
    lib.find_duplicates(None, &np, &cancel).unwrap();
    let safe = lib
        .auto_select_duplicates(&DupeFilter::default(), true)
        .unwrap();
    assert!(!safe.is_empty());
    let deduped = lib.dedupe(&safe, DedupeOp::Trash, &np, &cancel).unwrap();
    assert!(deduped.done > 0, "{:?}", deduped.errors);
    // .gitignores guarding junk are skipped, not trashed.
    assert!(copies[1].join("Projects/app/.gitignore").exists());
    assert!(!copies[1].join("Taxes/receipt (2).pdf").exists());

    // 5. Organise inside the vault: links follow the rename.
    let ideas = lib
        .search("Ideas.md", 10)
        .unwrap()
        .hits
        .into_iter()
        .find(|h| h.source_root == library.to_string_lossy())
        .unwrap()
        .file
        .id;
    let r = lib
        .organise(
            &[docgait_core::OrganiseOp::Rename {
                file_id: Some(ideas),
                folder_id: None,
                name: "Thoughts.md".into(),
            }],
            "rename",
        )
        .unwrap();
    assert_eq!(
        fs::read_to_string(library.join("Vault/Home.md")).unwrap(),
        "See [[Thoughts]]"
    );

    // 6. Undo everything, newest first: the copies and the library are as they started.
    for b in [r.batch_id, deduped.batch_id, cleaned.batch_id] {
        let u = lib.undo_batch(b).unwrap();
        assert_eq!(u.failed, 0, "batch {b}: {:?}", u.errors);
    }
    let history = lib.history(50).unwrap();
    let conflict_batch = history.iter().find(|b| b.kind == "conflict").unwrap().id;
    lib.undo_batch(conflict_batch).unwrap();
    lib.undo_batch(imported.batch_id).unwrap();
    assert!(copies[0].join("Taxes/2023/W2.pdf").exists());
    assert!(copies[1].join("Taxes/receipt (2).pdf").exists());
    let left: Vec<String> = tree(&library).into_iter().collect();
    assert!(left.is_empty(), "library should be empty again: {left:?}");
}
