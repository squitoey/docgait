//! The watcher reports real filesystem changes as folders to rescan.

use std::sync::mpsc;
use std::time::Duration;

use docgait_core::watch::{watch, Change};

#[test]
fn reports_changed_folder() {
    let dir = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    let (tx, rx) = mpsc::channel::<Vec<Change>>();
    let (_w, problems) = watch(vec![(1, root.clone())], move |c| {
        let _ = tx.send(c);
    })
    .unwrap();
    assert!(problems.is_empty(), "{problems:?}");
    // Give the OS watcher a moment to start (FSEvents has some latency).
    std::thread::sleep(Duration::from_millis(500));
    std::fs::write(root.join("docs/new.txt"), "x").unwrap();
    std::fs::write(root.join("docs/.DS_Store"), "junk").unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut seen = Vec::new();
    while std::time::Instant::now() < deadline {
        if let Ok(batch) = rx.recv_timeout(Duration::from_millis(500)) {
            seen.extend(batch);
            if seen.iter().any(covers_docs) {
                break;
            }
        }
    }
    assert!(
        seen.iter().any(covers_docs),
        "no change reported for docs/: {seen:?}"
    );
}

/// A rescan of `docs/` or of the root (FSEvents can also report the root folder, e.g. for its
/// creation just before watching started; the root covers everything below it).
fn covers_docs(c: &Change) -> bool {
    c.source_id == 1 && (c.rel == "docs" || c.rel.is_empty())
}
