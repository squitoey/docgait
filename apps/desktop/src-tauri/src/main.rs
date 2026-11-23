// Desktop shell: translates UI calls into docgait-core calls. No logic of its own.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use docgait_core::watch::{self, SourceWatcher};
use docgait_core::{
    scan, App, BatchInfo, CleanupResult, ConflictView, Coverage, Cursor, DedupeOp, DedupePlan,
    DedupeResult, DupeFilter, DupeGroup, DupePage, DupeScan, ExcludedSummary, ImportItem,
    ImportPlan, ImportProgress, ImportResult, IndexLevel, Layout, Library, LibraryInfo, Listing,
    Match, MergeStart, OrganiseOp, OrganisePlan, OrganiseResult, Resolution, Role, ScanProgress,
    ScanSummary, SearchResults, SortBy, Source, Status, TagCount, UndoResult,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

const LAST_LIBRARY: &str = "last_library_id";

type CmdResult<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    format!("{e:#}")
}

/// A request for the background scan worker. Scans run one at a time, off the UI thread.
struct ScanRequest {
    lib: Arc<Library>,
    source_id: Option<i64>,
    /// A folder inside the source (from the file watcher): a quick, quiet rescan of just that.
    scope: Option<String>,
}

struct AppState {
    app: Arc<App>,
    current: RwLock<Arc<Library>>,
    scan_tx: Sender<ScanRequest>,
    scanning: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    watcher: Mutex<Option<SourceWatcher>>,
    import_cancel: Arc<AtomicBool>,
}

impl AppState {
    fn lib(&self) -> Arc<Library> {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// (Re)starts watching the current library's online sources. Changes become quiet folder
    /// rescans on the scan worker.
    fn restart_watcher(&self) {
        let mut slot = self.watcher.lock().unwrap_or_else(|e| e.into_inner());
        *slot = None; // stop the old watcher first
        let lib = self.lib();
        let sources: Vec<(i64, PathBuf)> = lib
            .sources()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.online)
            .map(|s| (s.id, PathBuf::from(s.root)))
            .collect();
        if sources.is_empty() {
            return;
        }
        let tx = self.scan_tx.clone();
        let result = watch::watch(sources, move |changes| {
            for c in changes {
                let _ = tx.send(ScanRequest {
                    lib: lib.clone(),
                    source_id: Some(c.source_id),
                    scope: Some(c.rel),
                });
            }
        });
        match result {
            Ok((w, problems)) => {
                for p in problems {
                    eprintln!("not watching {p}");
                }
                *slot = Some(w);
            }
            Err(e) => eprintln!("file watching is off: {e:#}"),
        }
    }

    /// Full rescan of every source in the current library.
    fn rescan_all(&self) {
        let _ = self.scan_tx.send(ScanRequest {
            lib: self.lib(),
            source_id: None,
            scope: None,
        });
    }
}

#[derive(Serialize)]
struct AppInfo {
    libraries: Vec<LibraryInfo>,
    current: LibraryInfo,
    data_dir: String,
    scanning: bool,
}

#[derive(Serialize, Clone)]
struct ProgressEvent {
    library_id: i64,
    #[serde(flatten)]
    progress: ScanProgress,
}

#[derive(Serialize, Clone)]
struct FinishedEvent {
    library_id: i64,
    summary: Option<ScanSummary>,
    error: Option<String>,
}

#[derive(Serialize, Clone)]
struct ChangedEvent {
    library_id: i64,
}

#[tauri::command(async)]
fn app_info(state: State<AppState>) -> CmdResult<AppInfo> {
    Ok(AppInfo {
        libraries: state.app.libraries().map_err(err)?,
        current: state.lib().info.clone(),
        data_dir: state.app.data_dir().to_string_lossy().to_string(),
        scanning: state.scanning.load(Ordering::Relaxed),
    })
}

#[tauri::command(async)]
fn create_library(state: State<AppState>, name: String) -> CmdResult<LibraryInfo> {
    state.app.create_library(&name).map_err(err)
}

#[tauri::command(async)]
fn rename_library(state: State<AppState>, id: i64, name: String) -> CmdResult<()> {
    state.app.rename_library(id, &name).map_err(err)?;
    let lib = state.lib();
    if lib.info.id == id {
        // Reopen so the in-memory name matches.
        let reopened = Arc::new(state.app.open_library(id).map_err(err)?);
        *state.current.write().unwrap_or_else(|e| e.into_inner()) = reopened;
    }
    Ok(())
}

#[tauri::command(async)]
fn switch_library(state: State<AppState>, id: i64) -> CmdResult<LibraryInfo> {
    let lib = Arc::new(state.app.open_library(id).map_err(err)?);
    let info = lib.info.clone();
    *state.current.write().unwrap_or_else(|e| e.into_inner()) = lib;
    state
        .app
        .set_setting(LAST_LIBRARY, &id.to_string())
        .map_err(err)?;
    state.restart_watcher();
    state.rescan_all();
    Ok(info)
}

#[tauri::command(async)]
fn sources(state: State<AppState>) -> CmdResult<Vec<Source>> {
    state.lib().sources().map_err(err)
}

/// Adds a folder as a source and queues a scan of it.
#[tauri::command(async)]
fn add_source(
    state: State<AppState>,
    path: String,
    role: String,
    level: Option<String>,
) -> CmdResult<Source> {
    let role = Role::parse(&role).map_err(err)?;
    let level = level
        .map(|l| IndexLevel::parse(&l))
        .transpose()
        .map_err(err)?;
    let lib = state.lib();
    let source = lib.add_source(Path::new(&path), role, level).map_err(err)?;
    let _ = state.scan_tx.send(ScanRequest {
        lib,
        source_id: Some(source.id),
        scope: None,
    });
    state.restart_watcher();
    Ok(source)
}

#[tauri::command(async)]
fn remove_source(state: State<AppState>, id: i64) -> CmdResult<()> {
    if state.scanning.load(Ordering::Relaxed) {
        return Err("Wait for the scan to finish, or stop it, first.".into());
    }
    state.lib().remove_source(id).map_err(err)?;
    state.restart_watcher();
    Ok(())
}

#[tauri::command(async)]
fn set_source_level(state: State<AppState>, id: i64, level: String) -> CmdResult<()> {
    let level = IndexLevel::parse(&level).map_err(err)?;
    state.lib().set_source_level(id, level).map_err(err)
}

#[tauri::command(async)]
fn set_source_removable(state: State<AppState>, id: i64, removable: bool) -> CmdResult<()> {
    state.lib().set_source_removable(id, removable).map_err(err)
}

#[tauri::command(async)]
fn set_honor_ignore_files(state: State<AppState>, id: i64, on: bool) -> CmdResult<()> {
    state.lib().set_honor_ignore_files(id, on).map_err(err)
}

/// Queues a scan of one source, or of every source.
#[tauri::command]
fn scan(state: State<AppState>, source_id: Option<i64>) -> CmdResult<()> {
    state
        .scan_tx
        .send(ScanRequest {
            lib: state.lib(),
            source_id,
            scope: None,
        })
        .map_err(err)
}

#[tauri::command]
fn cancel_scan(state: State<AppState>) {
    state.cancel.store(true, Ordering::SeqCst);
}

#[tauri::command(async)]
fn list(
    state: State<AppState>,
    folder_id: i64,
    sort: Option<SortBy>,
    after: Option<Cursor>,
    limit: Option<usize>,
) -> CmdResult<Listing> {
    state
        .lib()
        .list(
            folder_id,
            sort.unwrap_or_default(),
            after.as_ref(),
            limit.unwrap_or(500),
        )
        .map_err(err)
}

#[tauri::command(async)]
fn library_folder(state: State<AppState>) -> CmdResult<Option<Source>> {
    state.lib().library_source().map_err(err)
}

/// Sets the current library's folder, then scans and watches it.
#[tauri::command(async)]
fn set_library_folder(state: State<AppState>, path: String) -> CmdResult<Source> {
    let lib = state.lib();
    let source = lib.set_library_folder(Path::new(&path)).map_err(err)?;
    state
        .app
        .set_library_root(lib.info.id, Path::new(&path))
        .map_err(err)?;
    let _ = state.scan_tx.send(ScanRequest {
        lib,
        source_id: Some(source.id),
        scope: None,
    });
    state.restart_watcher();
    Ok(source)
}

/// Throttled progress events for import planning and copying.
fn import_progress(handle: &AppHandle) -> impl Fn(&ImportProgress) + Sync + '_ {
    let last = Mutex::new(Instant::now() - Duration::from_secs(1));
    move |p: &ImportProgress| {
        let mut l = last.lock().unwrap_or_else(|e| e.into_inner());
        if l.elapsed() >= Duration::from_millis(200) || p.done == p.total {
            *l = Instant::now();
            let _ = handle.emit("import-progress", p.clone());
        }
    }
}

/// Builds an import plan (hashing what is needed). Nothing on disk changes.
#[tauri::command(async)]
fn plan_import(
    handle: AppHandle,
    state: State<AppState>,
    folder_ids: Vec<i64>,
    dest: String,
    layout: Layout,
) -> CmdResult<ImportPlan> {
    state.import_cancel.store(false, Ordering::SeqCst);
    let progress = import_progress(&handle);
    state
        .lib()
        .plan_import(&folder_ids, &dest, layout, &progress, &state.import_cancel)
        .map_err(err)
}

/// Carries out (or resumes) an import plan.
#[tauri::command(async)]
fn execute_import(
    handle: AppHandle,
    state: State<AppState>,
    run_id: i64,
) -> CmdResult<ImportResult> {
    state.import_cancel.store(false, Ordering::SeqCst);
    let lib = state.lib();
    let progress = import_progress(&handle);
    let res = lib
        .execute_import(run_id, &progress, &state.import_cancel)
        .map_err(err);
    let _ = handle.emit(
        "index-changed",
        ChangedEvent {
            library_id: lib.info.id,
        },
    );
    res
}

#[tauri::command]
fn cancel_import(state: State<AppState>) {
    state.import_cancel.store(true, Ordering::SeqCst);
}

#[tauri::command(async)]
fn import_plan(state: State<AppState>, run_id: i64) -> CmdResult<ImportPlan> {
    state.lib().import_plan(run_id).map_err(err)
}

#[tauri::command(async)]
fn import_items(
    state: State<AppState>,
    run_id: i64,
    action: Option<String>,
    conflicts_only: bool,
    offset: Option<usize>,
    limit: Option<usize>,
) -> CmdResult<Vec<ImportItem>> {
    state
        .lib()
        .import_items(
            run_id,
            action.as_deref(),
            conflicts_only,
            offset.unwrap_or(0),
            limit.unwrap_or(200),
        )
        .map_err(err)
}

/// How much of a folder (or whole source) is already in the library.
#[tauri::command(async)]
fn check_coverage(
    handle: AppHandle,
    state: State<AppState>,
    folder_id: i64,
) -> CmdResult<Coverage> {
    state.import_cancel.store(false, Ordering::SeqCst);
    let progress = import_progress(&handle);
    let lib = state.lib();
    let res = lib
        .check_coverage(folder_id, &progress, &state.import_cancel)
        .map_err(err);
    let _ = handle.emit(
        "index-changed",
        ChangedEvent {
            library_id: lib.info.id,
        },
    );
    res
}

#[tauri::command(async)]
fn coverage_files(
    state: State<AppState>,
    folder_id: i64,
    in_library: bool,
    limit: Option<usize>,
) -> CmdResult<Vec<Match>> {
    state
        .lib()
        .coverage_files(
            folder_id,
            in_library,
            limit.unwrap_or(1000),
            &|_| {},
            &state.import_cancel,
        )
        .map_err(err)
}

/// Trashes files verified to be in the library. Undoable.
#[tauri::command(async)]
fn cleanup(handle: AppHandle, state: State<AppState>, folder_id: i64) -> CmdResult<CleanupResult> {
    state.import_cancel.store(false, Ordering::SeqCst);
    let progress = import_progress(&handle);
    let lib = state.lib();
    let res = lib
        .cleanup(folder_id, &progress, &state.import_cancel)
        .map_err(err);
    let _ = handle.emit(
        "index-changed",
        ChangedEvent {
            library_id: lib.info.id,
        },
    );
    res
}

#[tauri::command(async)]
fn excluded(state: State<AppState>, source_id: i64) -> CmdResult<ExcludedSummary> {
    state.lib().excluded(source_id).map_err(err)
}

#[tauri::command(async)]
fn include_anyway(
    handle: AppHandle,
    state: State<AppState>,
    source_id: i64,
    rel: String,
    reason: String,
) -> CmdResult<()> {
    let lib = state.lib();
    lib.include_anyway(source_id, &rel, &reason).map_err(err)?;
    let _ = handle.emit(
        "index-changed",
        ChangedEvent {
            library_id: lib.info.id,
        },
    );
    Ok(())
}

#[tauri::command(async)]
fn conflicts(
    state: State<AppState>,
    include_resolved: Option<bool>,
) -> CmdResult<Vec<ConflictView>> {
    state
        .lib()
        .conflicts(include_resolved.unwrap_or(false))
        .map_err(err)
}

#[tauri::command(async)]
fn conflict_count(state: State<AppState>) -> CmdResult<i64> {
    state.lib().open_conflict_count().map_err(err)
}

#[tauri::command(async)]
fn resolve_conflict(
    handle: AppHandle,
    state: State<AppState>,
    id: i64,
    resolution: Resolution,
) -> CmdResult<i64> {
    let lib = state.lib();
    let res = lib.resolve_conflict(id, &resolution).map_err(err);
    let _ = handle.emit(
        "index-changed",
        ChangedEvent {
            library_id: lib.info.id,
        },
    );
    res
}

#[tauri::command(async)]
fn accept_suggestions(handle: AppHandle, state: State<AppState>) -> CmdResult<(i64, usize)> {
    let lib = state.lib();
    let res = lib.accept_suggestions().map_err(err);
    let _ = handle.emit(
        "index-changed",
        ChangedEvent {
            library_id: lib.info.id,
        },
    );
    res
}

#[tauri::command(async)]
fn merge_start(state: State<AppState>, id: i64, variant: String) -> CmdResult<MergeStart> {
    state.lib().merge_start(id, &variant).map_err(err)
}

/// Opens a conflict variant in its default app. Only paths that belong to a conflict.
#[tauri::command(async)]
fn open_conflict_file(state: State<AppState>, path: String) -> CmdResult<()> {
    let known = state
        .lib()
        .conflicts(true)
        .map_err(err)?
        .iter()
        .any(|c| c.variants.iter().any(|v| v.path == path));
    if !known {
        return Err("not a conflict file".into());
    }
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("/usr/bin/open")
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    cmd.arg(&path).spawn().map_err(err)?;
    Ok(())
}

#[tauri::command(async)]
fn plan_organise(state: State<AppState>, ops: Vec<OrganiseOp>) -> CmdResult<OrganisePlan> {
    state.lib().plan_organise(&ops).map_err(err)
}

#[tauri::command(async)]
fn organise(
    handle: AppHandle,
    state: State<AppState>,
    ops: Vec<OrganiseOp>,
    description: String,
) -> CmdResult<OrganiseResult> {
    let lib = state.lib();
    let res = lib.organise(&ops, &description).map_err(err);
    let _ = handle.emit(
        "index-changed",
        ChangedEvent {
            library_id: lib.info.id,
        },
    );
    res
}

#[tauri::command(async)]
fn preview_template(
    state: State<AppState>,
    folder_id: i64,
    dest_folder_id: i64,
    template: String,
) -> CmdResult<OrganisePlan> {
    let lib = state.lib();
    let ids = lib.file_ids_under(folder_id).map_err(err)?;
    lib.preview_template(&ids, dest_folder_id, &template)
        .map_err(err)
}

#[tauri::command(async)]
fn organise_by_template(
    handle: AppHandle,
    state: State<AppState>,
    folder_id: i64,
    dest_folder_id: i64,
    template: String,
) -> CmdResult<OrganiseResult> {
    let lib = state.lib();
    let ids = lib.file_ids_under(folder_id).map_err(err)?;
    let res = lib
        .organise_by_template(&ids, dest_folder_id, &template)
        .map_err(err);
    let _ = handle.emit(
        "index-changed",
        ChangedEvent {
            library_id: lib.info.id,
        },
    );
    res
}

/// The indexed folder for a path picked in the folder dialog.
#[tauri::command(async)]
fn folder_for_path(state: State<AppState>, path: String) -> CmdResult<i64> {
    state
        .lib()
        .folder_for_path(Path::new(&path))
        .map(|f| f.id)
        .map_err(err)
}

#[tauri::command(async)]
fn folder_path(state: State<AppState>, folder_id: i64) -> CmdResult<String> {
    state
        .lib()
        .folder_path(folder_id)
        .map(|p| p.to_string_lossy().to_string())
        .map_err(err)
}

#[tauri::command(async)]
fn tags(state: State<AppState>) -> CmdResult<Vec<TagCount>> {
    state.lib().tags().map_err(err)
}

#[tauri::command(async)]
fn file_tags(state: State<AppState>, file_id: i64) -> CmdResult<Vec<String>> {
    state.lib().file_tags(file_id).map_err(err)
}

#[tauri::command(async)]
fn add_tag(state: State<AppState>, file_ids: Vec<i64>, tag: String) -> CmdResult<()> {
    state.lib().add_tag(&file_ids, &tag).map_err(err)
}

#[tauri::command(async)]
fn remove_tag(state: State<AppState>, file_ids: Vec<i64>, tag: String) -> CmdResult<()> {
    state.lib().remove_tag(&file_ids, &tag).map_err(err)
}

/// Hashes what is needed to find exact duplicates (everything, or one folder's possible copies).
#[tauri::command(async)]
fn find_duplicates(
    handle: AppHandle,
    state: State<AppState>,
    folder_id: Option<i64>,
) -> CmdResult<DupeScan> {
    state.import_cancel.store(false, Ordering::SeqCst);
    let progress = import_progress(&handle);
    let lib = state.lib();
    let res = lib
        .find_duplicates(folder_id, &progress, &state.import_cancel)
        .map_err(err);
    let _ = handle.emit(
        "index-changed",
        ChangedEvent {
            library_id: lib.info.id,
        },
    );
    res
}

#[tauri::command(async)]
fn duplicate_groups(
    state: State<AppState>,
    filter: DupeFilter,
    offset: Option<usize>,
    limit: Option<usize>,
) -> CmdResult<DupePage> {
    state
        .lib()
        .duplicate_groups(&filter, offset.unwrap_or(0), limit.unwrap_or(100))
        .map_err(err)
}

#[tauri::command(async)]
fn duplicate_group_of(state: State<AppState>, file_id: i64) -> CmdResult<Option<DupeGroup>> {
    state.lib().duplicate_group_of(file_id).map_err(err)
}

#[tauri::command(async)]
fn duplicate_counts(
    state: State<AppState>,
    ids: Vec<i64>,
) -> CmdResult<std::collections::HashMap<i64, i64>> {
    state.lib().duplicate_counts(&ids).map_err(err)
}

#[tauri::command(async)]
fn auto_select_duplicates(
    state: State<AppState>,
    filter: DupeFilter,
    safe_only: bool,
) -> CmdResult<Vec<i64>> {
    state
        .lib()
        .auto_select_duplicates(&filter, safe_only)
        .map_err(err)
}

#[tauri::command(async)]
fn ignore_duplicate_group(state: State<AppState>, hash: String, ignore: bool) -> CmdResult<()> {
    state
        .lib()
        .ignore_duplicate_group(&hash, ignore)
        .map_err(err)
}

#[tauri::command(async)]
fn plan_dedupe(state: State<AppState>, ids: Vec<i64>, op: DedupeOp) -> CmdResult<DedupePlan> {
    state.lib().plan_dedupe(&ids, op).map_err(err)
}

/// Trashes or hard-links duplicates. Undoable.
#[tauri::command(async)]
fn dedupe(
    handle: AppHandle,
    state: State<AppState>,
    ids: Vec<i64>,
    op: DedupeOp,
) -> CmdResult<DedupeResult> {
    state.import_cancel.store(false, Ordering::SeqCst);
    let progress = import_progress(&handle);
    let lib = state.lib();
    let res = lib
        .dedupe(&ids, op, &progress, &state.import_cancel)
        .map_err(err);
    let _ = handle.emit(
        "index-changed",
        ChangedEvent {
            library_id: lib.info.id,
        },
    );
    res
}

#[tauri::command(async)]
fn history(state: State<AppState>) -> CmdResult<Vec<BatchInfo>> {
    state.lib().history(50).map_err(err)
}

#[tauri::command(async)]
fn undo_batch(handle: AppHandle, state: State<AppState>, batch_id: i64) -> CmdResult<UndoResult> {
    let lib = state.lib();
    let res = lib.undo_batch(batch_id).map_err(err);
    let _ = handle.emit(
        "index-changed",
        ChangedEvent {
            library_id: lib.info.id,
        },
    );
    res
}

/// Searches names and paths in the current library.
#[tauri::command(async)]
fn search(state: State<AppState>, query: String, limit: Option<usize>) -> CmdResult<SearchResults> {
    state
        .lib()
        .search(&query, limit.unwrap_or(200))
        .map_err(err)
}

#[tauri::command(async)]
fn status(state: State<AppState>) -> CmdResult<Status> {
    state.lib().status().map_err(err)
}

/// Opens a file in its default app. Only ids from the index are accepted, never raw paths.
#[tauri::command(async)]
fn open_file(state: State<AppState>, id: i64) -> CmdResult<()> {
    let path = state.lib().file_path(id).map_err(err)?;
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("/usr/bin/open")
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    cmd.arg(&path).spawn().map_err(err)?;
    Ok(())
}

/// Shows a file (or folder, when `folder` is true) in the system file manager.
#[tauri::command(async)]
fn reveal(state: State<AppState>, id: i64, folder: Option<bool>) -> CmdResult<()> {
    let lib = state.lib();
    let path = if folder.unwrap_or(false) {
        lib.folder_path(id)
    } else {
        lib.file_path(id)
    }
    .map_err(err)?;
    let mut cmd;
    if cfg!(target_os = "macos") {
        cmd = std::process::Command::new("/usr/bin/open");
        cmd.arg("-R").arg(&path);
    } else if cfg!(windows) {
        cmd = std::process::Command::new("explorer");
        cmd.arg(format!("/select,{}", path.display()));
    } else {
        // No portable "select this file" on Linux; open the folder it is in.
        cmd = std::process::Command::new("xdg-open");
        cmd.arg(if path.is_dir() {
            path.as_path()
        } else {
            path.parent().unwrap_or(&path)
        });
    }
    cmd.spawn().map_err(err)?;
    Ok(())
}

/// Runs queued scans one after another and reports progress as events.
fn scan_worker(
    handle: AppHandle,
    rx: Receiver<ScanRequest>,
    scanning: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
) {
    for req in rx {
        let library_id = req.lib.info.id;
        if let (Some(id), Some(rel)) = (req.source_id, req.scope.as_deref()) {
            // Watcher-driven: small, quiet, no progress. A failure (e.g. the source was just
            // removed) is not worth bothering anyone about; the next full scan settles it.
            let never = AtomicBool::new(false);
            if scan::scan_folder(&req.lib, id, rel, &|_| {}, &never).is_ok() {
                let _ = handle.emit("index-changed", ChangedEvent { library_id });
            }
            continue;
        }
        scanning.store(true, Ordering::SeqCst);
        cancel.store(false, Ordering::SeqCst);
        let last_emit = Mutex::new(Instant::now() - Duration::from_secs(1));
        let h = handle.clone();
        let progress = move |p: &ScanProgress| {
            // About four updates a second is plenty for a status bar.
            let mut last = last_emit.lock().unwrap_or_else(|e| e.into_inner());
            if last.elapsed() >= Duration::from_millis(250) {
                *last = Instant::now();
                let _ = h.emit(
                    "scan-progress",
                    ProgressEvent {
                        library_id,
                        progress: p.clone(),
                    },
                );
            }
        };
        let results: Vec<Result<ScanSummary, String>> = match req.source_id {
            Some(id) => vec![scan::scan_source(&req.lib, id, &progress, &cancel).map_err(err)],
            None => match req.lib.sources() {
                Ok(list) => list
                    .into_iter()
                    .filter(|s| s.online)
                    .take_while(|_| !cancel.load(Ordering::Relaxed))
                    .map(|s| scan::scan_source(&req.lib, s.id, &progress, &cancel).map_err(err))
                    .collect(),
                Err(e) => vec![Err(err(e))],
            },
        };
        for r in results {
            let (summary, error) = match r {
                Ok(s) => (Some(s), None),
                Err(e) => (None, Some(e)),
            };
            let _ = handle.emit(
                "scan-finished",
                FinishedEvent {
                    library_id,
                    summary,
                    error,
                },
            );
        }
        let _ = handle.emit("index-changed", ChangedEvent { library_id });
        scanning.store(false, Ordering::SeqCst);
    }
}

fn open_initial_library(app: &App) -> anyhow::Result<Library> {
    let wanted = app
        .setting(LAST_LIBRARY)?
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(|id| app.library_info(id).ok());
    let info = match wanted {
        Some(i) => i,
        None => app.default_library()?,
    };
    app.open_library(info.id)
}

fn main() {
    let app = Arc::new(App::open_default().expect("could not open the docgait data folder"));
    let lib = Arc::new(open_initial_library(&app).expect("could not open the library index"));
    let (scan_tx, scan_rx) = unbounded();
    let scanning = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            app,
            current: RwLock::new(lib),
            scan_tx,
            scanning: scanning.clone(),
            cancel: cancel.clone(),
            watcher: Mutex::new(None),
            import_cancel: Arc::new(AtomicBool::new(false)),
        })
        .setup(move |a| {
            let handle = a.handle().clone();
            std::thread::Builder::new()
                .name("scan-worker".into())
                .spawn(move || scan_worker(handle, scan_rx, scanning, cancel))?;
            // Catch up on changes made while the app was closed, then follow live changes.
            let state = a.state::<AppState>();
            state.rescan_all();
            state.restart_watcher();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            create_library,
            rename_library,
            switch_library,
            sources,
            add_source,
            remove_source,
            set_source_level,
            set_source_removable,
            set_honor_ignore_files,
            scan,
            cancel_scan,
            list,
            library_folder,
            set_library_folder,
            plan_import,
            execute_import,
            cancel_import,
            import_plan,
            import_items,
            history,
            undo_batch,
            plan_organise,
            organise,
            preview_template,
            organise_by_template,
            folder_for_path,
            folder_path,
            tags,
            file_tags,
            add_tag,
            remove_tag,
            conflicts,
            conflict_count,
            resolve_conflict,
            accept_suggestions,
            merge_start,
            open_conflict_file,
            check_coverage,
            coverage_files,
            cleanup,
            find_duplicates,
            duplicate_groups,
            duplicate_group_of,
            duplicate_counts,
            auto_select_duplicates,
            ignore_duplicate_group,
            plan_dedupe,
            dedupe,
            excluded,
            include_anyway,
            search,
            status,
            open_file,
            reveal
        ])
        .run(tauri::generate_context!())
        .expect("error while running docgait");
}
