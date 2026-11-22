//! Organise: moves keep ids and tags, templates, trash, undo, and Obsidian link updates.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use docgait_core::{scan, App, Library, OrganiseOp, SortBy};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn setup() -> (tempfile::TempDir, tempfile::TempDir, Library, PathBuf, i64) {
    static TRASH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let trash = TRASH.get_or_init(|| {
        std::env::temp_dir().join(format!("docgait-test-trash-{}", std::process::id()))
    });
    std::env::set_var("DOCGAIT_TRASH_DIR", trash);
    let data = tempfile::tempdir().unwrap();
    let dirs = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(dirs.path()).unwrap().join("Library");
    fs::create_dir_all(&root).unwrap();
    let app = App::open(data.path()).unwrap();
    let lib = app.open_library(app.default_library().unwrap().id).unwrap();
    let s = lib.set_library_folder(&root).unwrap();
    (data, dirs, lib, root, s.id)
}

fn rescan(lib: &Library, sid: i64) {
    scan::scan_source(lib, sid, &|_| {}, &AtomicBool::new(false)).unwrap();
}

fn folder(lib: &Library, root: &Path, rel: &str) -> i64 {
    lib.folder_for_path(&root.join(rel)).unwrap().id
}

fn file_id(lib: &Library, root: &Path, rel: &str) -> i64 {
    let p = root.join(rel);
    let f = lib.folder_for_path(p.parent().unwrap()).unwrap();
    let name = p.file_name().unwrap().to_string_lossy().to_string();
    lib.list(f.id, SortBy::Name, None, 5000)
        .unwrap()
        .files
        .into_iter()
        .find(|x| x.name == name)
        .unwrap()
        .id
}

#[test]
fn moves_keep_ids_and_tags_and_undo() {
    let (_d, _t, lib, root, sid) = setup();
    write(&root, "Inbox/W2.pdf", "w2");
    write(&root, "Inbox/W2 (from scan).pdf", "w2b");
    write(&root, "Taxes/W2.pdf", "existing");
    rescan(&lib, sid);
    let id = file_id(&lib, &root, "Inbox/W2.pdf");
    lib.add_tag(&[id], "taxes").unwrap();

    // Move into a folder that already has W2.pdf: the name is made unique.
    let ops = [OrganiseOp::Move {
        file_ids: vec![id],
        folder_ids: vec![],
        dest_folder_id: folder(&lib, &root, "Taxes"),
    }];
    let plan = lib.plan_organise(&ops).unwrap();
    assert!(plan.steps[0].to.as_ref().unwrap().ends_with("W2 (2).pdf"));
    let res = lib.organise(&ops, "test move").unwrap();
    assert_eq!(res.moved, 1, "{:?}", res.errors);
    assert!(root.join("Taxes/W2 (2).pdf").exists());
    // Same id, tag kept, search finds it at the new place.
    assert_eq!(file_id(&lib, &root, "Taxes/W2 (2).pdf"), id);
    assert_eq!(lib.file_tags(id).unwrap(), vec!["taxes"]);
    let hits = lib.search("tag:taxes", 10).unwrap().hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].folder_path, "Taxes");

    // Rename a folder: files below keep their ids; search paths follow.
    let res = lib
        .organise(
            &[OrganiseOp::Rename {
                file_id: None,
                folder_id: Some(folder(&lib, &root, "Taxes")),
                name: "Tax Returns".into(),
            }],
            "rename",
        )
        .unwrap();
    assert_eq!(res.failed, 0, "{:?}", res.errors);
    assert_eq!(file_id(&lib, &root, "Tax Returns/W2 (2).pdf"), id);
    assert_eq!(
        lib.search("tag:taxes", 10).unwrap().hits[0].folder_path,
        "Tax Returns"
    );
    assert_eq!(lib.search("returns w2", 10).unwrap().hits.len(), 2);

    // Undo the rename, then the move: back where it started, same id, tag kept.
    lib.undo_batch(res.batch_id).unwrap();
    let first = lib
        .history(10)
        .unwrap()
        .into_iter()
        .find(|b| b.description == "test move")
        .unwrap();
    lib.undo_batch(first.id).unwrap();
    assert!(root.join("Inbox/W2.pdf").exists());
    assert_eq!(file_id(&lib, &root, "Inbox/W2.pdf"), id);
    assert_eq!(lib.file_tags(id).unwrap(), vec!["taxes"]);
}

#[test]
fn mkdir_trash_and_refusals() {
    let (_d, _t, lib, root, sid) = setup();
    write(&root, "a.txt", "a");
    write(&root, "Docs/b.txt", "b");
    rescan(&lib, sid);
    let top = folder(&lib, &root, "");
    let res = lib
        .organise(
            &[OrganiseOp::Mkdir {
                parent_folder_id: top,
                name: "New".into(),
            }],
            "mkdir",
        )
        .unwrap();
    assert_eq!(res.created, 1);
    assert!(root.join("New").is_dir());
    assert!(lib.folder_for_path(&root.join("New")).is_ok());

    let res = lib
        .organise(
            &[OrganiseOp::Trash {
                file_ids: vec![file_id(&lib, &root, "a.txt")],
                folder_ids: vec![folder(&lib, &root, "Docs")],
            }],
            "trash",
        )
        .unwrap();
    assert_eq!(res.trashed, 2, "{:?}", res.errors);
    assert!(!root.join("a.txt").exists() && !root.join("Docs").exists());
    lib.undo_batch(res.batch_id).unwrap();
    assert!(root.join("a.txt").exists() && root.join("Docs/b.txt").exists());

    // Refusals: the source root, a folder into itself, a taken name.
    assert!(lib
        .plan_organise(&[OrganiseOp::Trash {
            file_ids: vec![],
            folder_ids: vec![top]
        }])
        .is_err());
    rescan(&lib, sid);
    let docs = folder(&lib, &root, "Docs");
    write(&root, "Docs/Sub/x.txt", "x");
    rescan(&lib, sid);
    let sub = folder(&lib, &root, "Docs/Sub");
    assert!(lib
        .plan_organise(&[OrganiseOp::Move {
            file_ids: vec![],
            folder_ids: vec![docs],
            dest_folder_id: sub
        }])
        .is_err());
    assert!(lib
        .plan_organise(&[OrganiseOp::Rename {
            file_id: None,
            folder_id: Some(docs),
            name: "New".into()
        }])
        .is_err());
}

#[test]
fn organise_by_template() {
    let (_d, _t, lib, root, sid) = setup();
    write(&root, "Inbox/report.pdf", "r");
    write(&root, "Inbox/photo.jpg", "p");
    write(&root, "Inbox/notes", "n");
    rescan(&lib, sid);
    let inbox = folder(&lib, &root, "Inbox");
    let ids = lib.file_ids_under(inbox).unwrap();
    let top = folder(&lib, &root, "");
    let preview = lib.preview_template(&ids, top, "{kind}/{ext}").unwrap();
    let tos: Vec<String> = preview.steps.iter().filter_map(|s| s.to.clone()).collect();
    assert!(
        tos.iter().any(|t| t.ends_with("document/pdf/report.pdf")),
        "{tos:?}"
    );
    assert!(tos.iter().any(|t| t.ends_with("image/jpg/photo.jpg")));
    assert!(tos.iter().any(|t| t.ends_with("other/no-extension/notes")));
    let res = lib.organise_by_template(&ids, top, "{kind}/{ext}").unwrap();
    assert_eq!(res.moved, 3, "{:?}", res.errors);
    assert!(root.join("document/pdf/report.pdf").exists());
    lib.undo_batch(res.batch_id).unwrap();
    assert!(root.join("Inbox/report.pdf").exists());
    assert!(!root.join("document").exists());
}

#[test]
fn obsidian_links_follow_moves() {
    let (_d, _t, lib, root, sid) = setup();
    fs::create_dir_all(root.join("Vault/.obsidian")).unwrap();
    write(
        &root,
        "Vault/Notes/Plan.md",
        "# Plan\n![chart](../Attachments/chart%201.png)\n",
    );
    write(
        &root,
        "Vault/Daily/today.md",
        "See [[Plan]] and [[Plan#Goals|goals]]. ![[chart 1.png]]\n",
    );
    write(&root, "Vault/Attachments/chart 1.png", "png");
    write(&root, "Vault/Projects/.keep", "");
    rescan(&lib, sid);

    // Rename the note: wikilinks follow.
    let plan_id = file_id(&lib, &root, "Vault/Notes/Plan.md");
    let ops = [OrganiseOp::Rename {
        file_id: Some(plan_id),
        folder_id: None,
        name: "Roadmap.md".into(),
    }];
    let plan = lib.plan_organise(&ops).unwrap();
    assert_eq!(plan.notes_to_update.len(), 1, "{plan:?}");
    let res = lib.organise(&ops, "rename note").unwrap();
    assert_eq!(res.notes_updated, 1, "{:?}", res.errors);
    let today = fs::read_to_string(root.join("Vault/Daily/today.md")).unwrap();
    assert_eq!(
        today,
        "See [[Roadmap]] and [[Roadmap#Goals|goals]]. ![[chart 1.png]]\n"
    );

    // Move the note to another folder: its own relative image link is rewritten.
    let ops = [OrganiseOp::Move {
        file_ids: vec![plan_id],
        folder_ids: vec![],
        dest_folder_id: folder(&lib, &root, "Vault/Projects"),
    }];
    lib.organise(&ops, "move note").unwrap();
    assert_eq!(
        fs::read_to_string(root.join("Vault/Projects/Roadmap.md")).unwrap(),
        "# Plan\n![chart](../Attachments/chart%201.png)\n"
    );
    // Move the attachment: both the wikilink embed (by name, still unique) and the markdown link follow.
    let chart = file_id(&lib, &root, "Vault/Attachments/chart 1.png");
    let res = lib
        .organise(
            &[OrganiseOp::Move {
                file_ids: vec![chart],
                folder_ids: vec![],
                dest_folder_id: folder(&lib, &root, "Vault/Projects"),
            }],
            "move chart",
        )
        .unwrap();
    assert_eq!(res.notes_updated, 1, "{:?}", res.errors);
    assert_eq!(
        fs::read_to_string(root.join("Vault/Projects/Roadmap.md")).unwrap(),
        "# Plan\n![chart](chart%201.png)\n"
    );

    // Undo restores the note text.
    lib.undo_batch(res.batch_id).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("Vault/Projects/Roadmap.md")).unwrap(),
        "# Plan\n![chart](../Attachments/chart%201.png)\n"
    );
    assert!(root.join("Vault/Attachments/chart 1.png").exists());

    // Leaving the vault warns.
    let plan = lib
        .plan_organise(&[OrganiseOp::Move {
            file_ids: vec![plan_id],
            folder_ids: vec![],
            dest_folder_id: folder(&lib, &root, ""),
        }])
        .unwrap();
    assert!(plan
        .warnings
        .iter()
        .any(|w| w.contains("leaves the Obsidian vault")));
}
