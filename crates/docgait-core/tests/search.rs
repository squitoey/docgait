//! Filename/path search against a real scanned tree.

use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use docgait_core::{scan, App, Library, Role};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn names(lib: &Library, q: &str) -> Vec<String> {
    let mut v: Vec<String> = lib
        .search(q, 100)
        .unwrap()
        .hits
        .into_iter()
        .map(|h| {
            if h.folder_path.is_empty() {
                h.file.name
            } else {
                format!("{}/{}", h.folder_path, h.file.name)
            }
        })
        .collect();
    v.sort();
    v
}

#[test]
fn finds_by_name_path_and_filters() {
    let data = tempfile::tempdir().unwrap();
    let tree = tempfile::tempdir().unwrap();
    let t = tree.path();
    write(t, "Taxes/2023/W2.pdf", "pdf");
    write(t, "Taxes/2023/1099-INT.pdf", "pdf");
    write(t, "Taxes/2024/W2 draft.pdf", "pdf");
    write(t, "Photos/IMG_1234.jpg", &"x".repeat(5000));
    write(t, "Photos/IMG_1234 (2).jpg", "x");
    write(t, "Notes/lease renewal notice.txt", "x");
    write(t, "Notes/Résumé.docx", "x");

    let app = App::open(data.path()).unwrap();
    let lib = app.open_library(app.default_library().unwrap().id).unwrap();
    let s = lib.add_source(t, Role::Import, None).unwrap();
    scan::scan_source(&lib, s.id, &|_| {}, &AtomicBool::new(false)).unwrap();

    // Substring of a name.
    assert_eq!(
        names(&lib, "1234"),
        vec!["Photos/IMG_1234 (2).jpg", "Photos/IMG_1234.jpg"]
    );
    // Words can match the folder path: "taxes 2023" finds files in Taxes/2023.
    assert_eq!(
        names(&lib, "taxes 2023"),
        vec!["Taxes/2023/1099-INT.pdf", "Taxes/2023/W2.pdf"]
    );
    // Short terms (under three characters) still work.
    assert_eq!(
        names(&lib, "w2"),
        vec!["Taxes/2023/W2.pdf", "Taxes/2024/W2 draft.pdf"]
    );
    // Exclusion, of a long and a short term.
    assert_eq!(names(&lib, "w2 -draft"), vec!["Taxes/2023/W2.pdf"]);
    assert_eq!(names(&lib, "taxes -w2"), vec!["Taxes/2023/1099-INT.pdf"]);
    // Phrases.
    assert_eq!(
        names(&lib, "\"renewal notice\""),
        vec!["Notes/lease renewal notice.txt"]
    );
    assert!(names(&lib, "\"notice renewal\"").is_empty());
    // Filters.
    assert_eq!(
        names(&lib, "type:jpg size:>1kb"),
        vec!["Photos/IMG_1234.jpg"]
    );
    assert_eq!(names(&lib, "kind:document in:Notes").len(), 2);
    assert_eq!(names(&lib, "in:2024"), vec!["Taxes/2024/W2 draft.pdf"]);
    // Accents are ignored.
    assert_eq!(names(&lib, "resume"), vec!["Notes/Résumé.docx"]);
    // Removed files drop out of search after a rescan.
    fs::remove_file(t.join("Taxes/2023/W2.pdf")).unwrap();
    scan::scan_source(&lib, s.id, &|_| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!(names(&lib, "w2"), vec!["Taxes/2024/W2 draft.pdf"]);
    // Empty and filter-only-unsupported queries return nothing, with a note.
    assert!(lib.search("", 10).unwrap().hits.is_empty());
    let r = lib.search("tag:receipts", 10).unwrap();
    assert!(r.hits.is_empty());
    assert_eq!(r.unsupported.len(), 1);
}
