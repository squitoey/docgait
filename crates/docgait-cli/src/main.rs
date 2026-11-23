//! `docgait` command-line tool: a thin layer over docgait-core. Shares the index with the app.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use docgait_core::{scan, App, ImportProgress, IndexLevel, Layout, Library, Role, SortBy};

#[derive(Parser)]
#[command(
    name = "docgait",
    version,
    about = "Index, browse and organise your files"
)]
struct Cli {
    /// Keep all data under this folder instead of the OS default locations.
    #[arg(long, global = true, env = "DOCGAIT_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Library to work on (name). Defaults to the first library.
    #[arg(long, short = 'L', global = true, env = "DOCGAIT_LIBRARY")]
    library: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Add a folder as a source (if needed) and scan it; with no folder, rescan every source.
    Scan {
        path: Option<PathBuf>,
        /// Role for a newly added source.
        #[arg(long, value_enum, default_value_t = RoleArg::Import)]
        role: RoleArg,
        /// Index level for a newly added source (default: names, or full for the library folder).
        #[arg(long, value_enum)]
        level: Option<LevelArg>,
    },
    /// Manage sources.
    Sources {
        #[command(subcommand)]
        cmd: Option<SourcesCmd>,
    },
    /// List a folder from the index.
    Ls {
        /// Folder path inside a source. Without it, lists the sources.
        path: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = SortArg::Name)]
        sort: SortArg,
        /// Maximum number of files to show.
        #[arg(long, default_value_t = 200)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Search file names and folder paths. Words must all match; filters: type: kind: in:
    /// after: before: created-after: created-before: size:>10mb; -word excludes; "a phrase".
    Search {
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show or set this library's folder (where imports go).
    LibraryFolder { path: Option<PathBuf> },
    /// Import folders into the library folder. A dry run unless --yes is given.
    Import {
        /// Folders (inside sources) to import; a source root imports the whole source.
        #[arg(num_args = 0..)]
        folders: Vec<PathBuf>,
        /// Folder inside the library to import into (default: the library root).
        #[arg(long, default_value = "")]
        dest: String,
        /// merge: combine matching paths; own-folder: each imported folder gets its own folder.
        #[arg(long, value_enum, default_value_t = LayoutArg::Merge)]
        layout: LayoutArg,
        /// Carry out the plan.
        #[arg(long)]
        yes: bool,
        /// Run (or resume) a plan made earlier instead of making a new one.
        #[arg(long)]
        resume: Option<i64>,
        /// Also list every file to be copied.
        #[arg(long)]
        list: bool,
    },
    /// How much of a folder (or whole source) is already in the library.
    Check {
        path: PathBuf,
        /// List the files that are NOT in the library yet.
        #[arg(long)]
        missing: bool,
    },
    /// Trash files in a folder (or source) that are verified to be in the library. A dry run
    /// unless --yes is given. Files the library does not have are never touched.
    Cleanup {
        path: PathBuf,
        #[arg(long)]
        yes: bool,
    },
    /// Find exact duplicates (hashing only files that share a size) and list the groups.
    Dupes {
        /// Only files that could be copies of something in this folder (default: everything).
        path: Option<PathBuf>,
        /// Only groups with copies on import sources that the library already has.
        #[arg(long)]
        safe: bool,
        /// Only this kind (document, image, audio, ...).
        #[arg(long)]
        kind: Option<String>,
        /// Show at most this many groups.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// List the stored groups without hashing anything new.
        #[arg(long)]
        no_find: bool,
        #[arg(long)]
        json: bool,
    },
    /// Remove duplicates found by `docgait dupes`, keeping the suggested copy of each group.
    /// A dry run unless --yes. The last copy of anything is never removed.
    Dedupe {
        /// Only groups touching this folder (default: everything).
        path: Option<PathBuf>,
        /// Only copies on import sources that the library already has.
        #[arg(long)]
        safe: bool,
        /// Replace copies with hard links instead of trashing them (same drive only).
        #[arg(long)]
        hardlink: bool,
        #[arg(long)]
        yes: bool,
    },
    /// What a source's scan left out, and why.
    Excluded {
        /// The source folder.
        path: PathBuf,
    },
    /// Bring back something the scan left out (see `docgait excluded`).
    IncludeAnyway {
        /// The excluded file or folder.
        path: PathBuf,
    },
    /// Open conflicts in the library (same path, different contents).
    Conflicts,
    /// Resolve a conflict: keep-current, keep <variant path>, or mark (close without changes).
    Resolve {
        id: i64,
        #[arg(value_enum)]
        how: ResolveArg,
        /// The version to keep (for `keep`).
        path: Option<String>,
    },
    /// Keep the suggested version of every conflict that has a clear suggestion.
    AcceptSuggestions {
        #[arg(long)]
        yes: bool,
    },
    /// Move files or folders (inside sources) into a folder. A dry run unless --yes.
    Mv {
        #[arg(required = true, num_args = 2..)]
        paths: Vec<PathBuf>,
        #[arg(long)]
        yes: bool,
    },
    /// Rename a file or folder.
    Rename { path: PathBuf, new_name: String },
    /// Create a folder inside a source.
    Mkdir { path: PathBuf },
    /// Move the files under a folder into a structure made from a template, e.g.
    /// "{kind}/{year}/{month}". Tokens: {year} {month} {day} {kind} {ext} {tag} {folder} {source}.
    Organise {
        folder: PathBuf,
        template: String,
        /// Where the structure goes (default: the folder itself).
        #[arg(long)]
        into: Option<PathBuf>,
        #[arg(long)]
        yes: bool,
    },
    /// Tag files: `docgait tag add receipts a.pdf b.pdf`, `remove`, or `list` (all tags).
    Tag {
        #[arg(value_enum)]
        action: TagAction,
        /// The tag (not needed for list).
        tag: Option<String>,
        files: Vec<PathBuf>,
    },
    /// Recent imports and other undoable actions.
    History,
    /// Undo an import (or other action) by its batch number from `docgait history`.
    Undo {
        batch: i64,
        #[arg(long)]
        yes: bool,
    },
    /// Counts for the current library.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Manage libraries.
    Libraries {
        #[command(subcommand)]
        cmd: Option<LibrariesCmd>,
    },
}

#[derive(Subcommand)]
enum SourcesCmd {
    /// List sources (the default).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Forget a source. Its files are not touched.
    Remove { id: i64 },
    /// Change a source's settings.
    Set {
        id: i64,
        #[arg(long, value_enum)]
        level: Option<LevelArg>,
        /// Keep files that cannot be found (drives that come and go).
        #[arg(long)]
        removable: Option<OnOff>,
        /// Honour .gitignore, .ignore, .rgignore and .docgaitignore.
        #[arg(long)]
        ignore_files: Option<OnOff>,
        /// Extra exclude pattern (gitignore syntax). Repeatable; replaces the current list.
        #[arg(long = "exclude")]
        excludes: Option<Vec<String>>,
        /// Bring back something the default excludes skip (e.g. .vscode). Repeatable; replaces the current list.
        #[arg(long = "include")]
        includes: Option<Vec<String>>,
    },
}

#[derive(Subcommand)]
enum LibrariesCmd {
    /// List libraries (the default).
    List,
    /// Create a library.
    Create { name: String },
    /// Rename a library.
    Rename { id: i64, name: String },
    /// Set the library folder.
    Root { id: i64, path: PathBuf },
    /// Forget a library: deletes its index, never your files.
    Remove {
        id: i64,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum RoleArg {
    Library,
    Import,
    Indexed,
}

#[derive(Clone, Copy, ValueEnum)]
enum LevelArg {
    Names,
    Full,
    Agent,
}

#[derive(Clone, Copy, ValueEnum)]
enum TagAction {
    Add,
    Remove,
    List,
}

#[derive(Clone, Copy, ValueEnum)]
enum ResolveArg {
    KeepCurrent,
    Keep,
    Mark,
}

#[derive(Clone, Copy, ValueEnum)]
enum LayoutArg {
    Merge,
    OwnFolder,
}

#[derive(Clone, Copy, ValueEnum)]
enum SortArg {
    Name,
    Modified,
    Size,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

impl From<RoleArg> for Role {
    fn from(r: RoleArg) -> Self {
        match r {
            RoleArg::Library => Role::Library,
            RoleArg::Import => Role::Import,
            RoleArg::Indexed => Role::Indexed,
        }
    }
}

impl From<LevelArg> for IndexLevel {
    fn from(l: LevelArg) -> Self {
        match l {
            LevelArg::Names => IndexLevel::Names,
            LevelArg::Full => IndexLevel::Full,
            LevelArg::Agent => IndexLevel::Agent,
        }
    }
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let app = match &cli.data_dir {
        Some(d) => App::open(d)?,
        None => App::open_default()?,
    };
    if let Cmd::Libraries { cmd } = &cli.cmd {
        return libraries(&app, cmd.as_ref());
    }
    let info = match &cli.library {
        Some(name) => app.find_library(name)?,
        None => app.default_library()?,
    };
    let lib = app.open_library(info.id)?;
    match cli.cmd {
        Cmd::Scan { path, role, level } => scan_cmd(&lib, path, role.into(), level.map(Into::into)),
        Cmd::Sources { cmd } => sources(&lib, cmd),
        Cmd::Ls {
            path,
            sort,
            limit,
            json,
        } => ls(&lib, path, sort, limit, json),
        Cmd::Search { query, limit, json } => {
            let started = std::time::Instant::now();
            let r = lib.search(&query.join(" "), limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            for note in &r.unsupported {
                eprintln!("note: ignored {note}");
            }
            for h in &r.hits {
                let path = PathBuf::from(&h.source_root)
                    .join(&h.folder_path)
                    .join(&h.file.name);
                println!("{:>10}  {}", human(h.file.size), path.display());
            }
            eprintln!(
                "{} result{}{} in {} ms",
                r.hits.len(),
                if r.hits.len() == 1 { "" } else { "s" },
                if r.more { " (more: use --limit)" } else { "" },
                started.elapsed().as_millis()
            );
            Ok(())
        }
        Cmd::LibraryFolder { path } => {
            match path {
                Some(p) => {
                    let s = lib.set_library_folder(&p)?;
                    app.set_library_root(lib.info.id, &p)?;
                    println!("library folder: {}", s.root);
                    if !scan::has_scanned(&lib, s.id)? {
                        let cancel = AtomicBool::new(false);
                        let sum = scan::scan_source(&lib, s.id, &|_| {}, &cancel)?;
                        println!("scanned: {} files", sum.files);
                    }
                }
                None => match lib.library_source()? {
                    Some(s) => println!("{}", s.root),
                    None => println!("no library folder yet: docgait library-folder <path>"),
                },
            }
            Ok(())
        }
        Cmd::Import {
            folders,
            dest,
            layout,
            yes,
            resume,
            list,
        } => import_cmd(&lib, folders, &dest, layout, yes, resume, list),
        Cmd::Check { path, missing } => {
            let folder = lib.folder_for_path(&path)?;
            let cancel = AtomicBool::new(false);
            let progress = |p: &ImportProgress| {
                if p.total > 0 {
                    eprint!("\r{}: {}/{}   ", p.phase, p.done, p.total);
                }
            };
            let c = lib.check_coverage(folder.id, &progress, &cancel)?;
            eprintln!();
            let pct = if c.files > 0 {
                c.in_library * 100 / c.files
            } else {
                100
            };
            println!(
                "{pct}% in the library: {} of {} files ({}); {} not yet ({})",
                c.in_library,
                c.files,
                human(c.bytes_in_library),
                c.not_in_library,
                human(c.bytes_not_in_library)
            );
            if missing {
                for m in lib.coverage_files(folder.id, false, 100_000, &|_| {}, &cancel)? {
                    println!("  {}", m.path);
                }
            }
            Ok(())
        }
        Cmd::Cleanup { path, yes } => {
            let folder = lib.folder_for_path(&path)?;
            let cancel = AtomicBool::new(false);
            let progress = |p: &ImportProgress| {
                if p.total > 0 {
                    eprint!("\r{}: {}/{}   ", p.phase, p.done, p.total);
                }
            };
            if !yes {
                let c = lib.check_coverage(folder.id, &progress, &cancel)?;
                eprintln!();
                println!(
                    "would trash {} files ({}) that are verified to be in the library; {} files are not and stay. Add --yes to do it.",
                    c.in_library,
                    human(c.bytes_in_library),
                    c.not_in_library
                );
                return Ok(());
            }
            let r = lib.cleanup(folder.id, &progress, &cancel)?;
            eprintln!();
            println!(
                "trashed {} files ({}), removed {} empty folders, skipped {}. Undo with `docgait undo {}`.",
                r.trashed,
                human(r.bytes as i64),
                r.folders_removed,
                r.skipped,
                r.batch_id
            );
            if r.kept_ignore_files > 0 {
                println!(
                    "kept {} ignore file(s): their folders still hold excluded files",
                    r.kept_ignore_files
                );
            }
            for e in r.errors {
                println!("  {e}");
            }
            Ok(())
        }
        Cmd::Dupes {
            path,
            safe,
            kind,
            limit,
            no_find,
            json,
        } => {
            let folder = path.map(|p| lib.folder_for_path(&p)).transpose()?;
            if !no_find {
                let progress = |p: &ImportProgress| {
                    if p.total > 0 {
                        eprint!("\r{}: {}/{}   ", p.phase, p.done, p.total);
                    }
                };
                let f = lib.find_duplicates(
                    folder.as_ref().map(|f| f.id),
                    &progress,
                    &AtomicBool::new(false),
                )?;
                eprint!("\r\x1b[K");
                eprintln!("compared {} files", f.files);
            }
            let filter = docgait_core::DupeFilter {
                folder_id: folder.map(|f| f.id),
                kind,
                safe_only: safe,
                ..Default::default()
            };
            let page = lib.duplicate_groups(&filter, 0, limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&page)?);
                return Ok(());
            }
            println!(
                "{} groups, {} files, {} reclaimable; {} copies ({}) on import sources are already in the library",
                page.total_groups,
                page.total_files,
                human(page.total_reclaimable),
                page.safe_files,
                human(page.safe_bytes)
            );
            for g in &page.groups {
                println!("\n{} × {} ({})", g.members.len(), human(g.size), g.kind);
                for m in &g.members {
                    let mut tags = Vec::new();
                    if m.keeper {
                        tags.push("keep");
                    }
                    if m.safe {
                        tags.push("safe");
                    }
                    if m.hardlinked {
                        tags.push("linked");
                    }
                    if m.offline {
                        tags.push("offline");
                    }
                    let mark = if m.keeper { "*" } else { " " };
                    let t = if tags.is_empty() {
                        String::new()
                    } else {
                        format!("  [{}]", tags.join(", "))
                    };
                    println!("  {mark} {}{t}", m.path);
                }
            }
            if page.total_groups > page.groups.len() {
                println!(
                    "\n… {} more groups (--limit)",
                    page.total_groups - page.groups.len()
                );
            }
            Ok(())
        }
        Cmd::Dedupe {
            path,
            safe,
            hardlink,
            yes,
        } => {
            let folder = path.map(|p| lib.folder_for_path(&p)).transpose()?;
            let filter = docgait_core::DupeFilter {
                folder_id: folder.map(|f| f.id),
                ..Default::default()
            };
            let ids = lib.auto_select_duplicates(&filter, safe)?;
            let op = if hardlink {
                docgait_core::DedupeOp::Hardlink
            } else {
                docgait_core::DedupeOp::Trash
            };
            let plan = lib.plan_dedupe(&ids, op)?;
            let verb = if hardlink { "hard-link" } else { "trash" };
            if !yes {
                for i in plan.items.iter().filter(|i| i.status == "ok").take(50) {
                    println!("  {}  (keeps {})", i.path, i.keep.as_deref().unwrap_or(""));
                }
                println!(
                    "would {verb} {} files ({}); {} skipped. Add --yes to do it.",
                    plan.ok,
                    human(plan.bytes),
                    plan.skipped
                );
                return Ok(());
            }
            let progress = |p: &ImportProgress| {
                if p.total > 0 {
                    eprint!("\r{}: {}/{}   ", p.phase, p.done, p.total);
                }
            };
            let r = lib.dedupe(&ids, op, &progress, &AtomicBool::new(false))?;
            eprint!("\r\x1b[K");
            println!(
                "{verb}: {} files ({}), skipped {}. Undo with `docgait undo {}`.",
                r.done,
                human(r.bytes),
                r.skipped,
                r.batch_id
            );
            for e in r.errors {
                println!("  {e}");
            }
            Ok(())
        }
        Cmd::Excluded { path } => {
            let folder = lib.folder_for_path(&path)?;
            let ex = lib.excluded(folder.source_id)?;
            for (reason, n) in &ex.by_reason {
                println!("{n:>6}  {}", reason_text(reason));
            }
            for e in &ex.entries {
                println!(
                    "  {}{}  [{}]",
                    e.rel,
                    if e.is_dir { "/" } else { "" },
                    e.reason
                );
            }
            if ex.truncated {
                println!("  … more");
            }
            Ok(())
        }
        Cmd::IncludeAnyway { path } => {
            let abs = std::path::absolute(&path)?;
            let source = lib
                .sources()?
                .into_iter()
                .find(|s| abs.starts_with(&s.root))
                .ok_or_else(|| anyhow::anyhow!("{} is not inside a source", path.display()))?;
            let rel = abs
                .strip_prefix(&source.root)?
                .to_string_lossy()
                .replace('\\', "/");
            let ex = lib.excluded(source.id)?;
            let entry = ex.entries.iter().find(|e| e.rel == rel).ok_or_else(|| {
                anyhow::anyhow!(
                    "{rel} is not excluded (or is inside an excluded folder: name that folder)"
                )
            })?;
            lib.include_anyway(source.id, &rel, entry.reason)?;
            println!("{rel} is indexed now");
            Ok(())
        }
        Cmd::Conflicts => {
            let cs = lib.conflicts(false)?;
            if cs.is_empty() {
                println!("no open conflicts");
            }
            for c in cs {
                println!("{:>4}  [{}]  {}", c.id, c.state, c.path);
                for v in c.variants {
                    println!(
                        "        {} {}  {}  ({}, {})",
                        if v.is_current { "*" } else { " " },
                        v.path,
                        v.origin,
                        human(v.size),
                        if v.exists { "here" } else { "missing" }
                    );
                }
            }
            Ok(())
        }
        Cmd::Resolve { id, how, path } => {
            let r = match how {
                ResolveArg::KeepCurrent => docgait_core::Resolution::KeepCurrent,
                ResolveArg::Mark => docgait_core::Resolution::MarkResolved,
                ResolveArg::Keep => docgait_core::Resolution::KeepVariant {
                    path: path.ok_or_else(|| anyhow::anyhow!("name the version to keep"))?,
                },
            };
            let batch = lib.resolve_conflict(id, &r)?;
            println!("resolved conflict {id}. Undo with `docgait undo {batch}`.");
            Ok(())
        }
        Cmd::AcceptSuggestions { yes } => {
            if !yes {
                let n = lib
                    .conflicts(false)?
                    .iter()
                    .filter(|c| c.state == "suggested")
                    .count();
                println!("would keep the suggested version of {n} conflicts (the others go to the trash). Add --yes.");
                return Ok(());
            }
            let (batch, n) = lib.accept_suggestions()?;
            println!("resolved {n} conflicts. Undo with `docgait undo {batch}`.");
            Ok(())
        }
        Cmd::Mv { mut paths, yes } => {
            let dest = paths.pop().expect("clap guarantees two paths");
            let dest_id = lib.folder_for_path(&dest)?.id;
            let (mut file_ids, mut folder_ids) = (Vec::new(), Vec::new());
            for p in &paths {
                if p.is_dir() {
                    folder_ids.push(lib.folder_for_path(p)?.id);
                } else {
                    file_ids.push(file_id_for(&lib, p)?);
                }
            }
            let ops = [docgait_core::OrganiseOp::Move {
                file_ids,
                folder_ids,
                dest_folder_id: dest_id,
            }];
            run_ops(&lib, &ops, "Move", yes)
        }
        Cmd::Rename { path, new_name } => {
            let op = if path.is_dir() {
                docgait_core::OrganiseOp::Rename {
                    file_id: None,
                    folder_id: Some(lib.folder_for_path(&path)?.id),
                    name: new_name,
                }
            } else {
                docgait_core::OrganiseOp::Rename {
                    file_id: Some(file_id_for(&lib, &path)?),
                    folder_id: None,
                    name: new_name,
                }
            };
            run_ops(&lib, &[op], "Rename", true)
        }
        Cmd::Mkdir { path } => {
            let parent = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("no parent folder"))?;
            let name = path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("no folder name"))?
                .to_string_lossy()
                .to_string();
            let op = docgait_core::OrganiseOp::Mkdir {
                parent_folder_id: lib.folder_for_path(parent)?.id,
                name,
            };
            run_ops(&lib, &[op], "New folder", true)
        }
        Cmd::Organise {
            folder,
            template,
            into,
            yes,
        } => {
            let f = lib.folder_for_path(&folder)?;
            let dest = match into {
                Some(p) => lib.folder_for_path(&p)?.id,
                None => f.id,
            };
            let ids = lib.file_ids_under(f.id)?;
            if !yes {
                let plan = lib.preview_template(&ids, dest, &template)?;
                print_plan(&plan);
                println!("dry run: add --yes to do it.");
                return Ok(());
            }
            let r = lib.organise_by_template(&ids, dest, &template)?;
            print_result(&r);
            Ok(())
        }
        Cmd::Tag { action, tag, files } => match action {
            TagAction::List => {
                for t in lib.tags()? {
                    println!("{:>6}  {}", t.files, t.name);
                }
                Ok(())
            }
            TagAction::Add | TagAction::Remove => {
                let tag = tag.ok_or_else(|| anyhow::anyhow!("name the tag"))?;
                let ids = files
                    .iter()
                    .map(|p| file_id_for(&lib, p))
                    .collect::<Result<Vec<_>>>()?;
                if matches!(action, TagAction::Add) {
                    lib.add_tag(&ids, &tag)?;
                } else {
                    lib.remove_tag(&ids, &tag)?;
                }
                println!(
                    "{} {} file(s)",
                    if matches!(action, TagAction::Add) {
                        "tagged"
                    } else {
                        "untagged"
                    },
                    ids.len()
                );
                Ok(())
            }
        },
        Cmd::History => {
            let h = lib.history(30)?;
            if h.is_empty() {
                println!("nothing yet");
            }
            for b in h {
                println!(
                    "{:>4}  {}  {:<8} {:<8} {}  ({} done{})",
                    b.id,
                    fmt_time(b.created_at),
                    b.kind,
                    b.state,
                    b.description,
                    b.done,
                    if b.undone > 0 {
                        format!(", {} undone", b.undone)
                    } else {
                        String::new()
                    }
                );
            }
            Ok(())
        }
        Cmd::Undo { batch, yes } => {
            if !yes {
                let b = lib
                    .history(1000)?
                    .into_iter()
                    .find(|b| b.id == batch)
                    .ok_or_else(|| anyhow::anyhow!("no batch {batch}"))?;
                println!(
                    "would undo batch {}: {} ({} file operations). Copies go to the trash. Add --yes to do it.",
                    b.id, b.description, b.done
                );
                return Ok(());
            }
            let r = lib.undo_batch(batch)?;
            println!("undone: {} operations, {} failed", r.undone, r.failed);
            for e in r.errors {
                println!("  {e}");
            }
            Ok(())
        }
        Cmd::Status { json } => {
            let s = lib.status()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                println!("library  {}", lib.info.name);
                println!("sources  {}", s.sources);
                println!("folders  {}", s.folders);
                println!("files    {}  ({})", s.files, human(s.bytes));
                if s.missing > 0 {
                    println!("missing  {}", s.missing);
                }
            }
            Ok(())
        }
        Cmd::Libraries { .. } => unreachable!(),
    }
}

fn scan_cmd(
    lib: &Library,
    path: Option<PathBuf>,
    role: Role,
    level: Option<IndexLevel>,
) -> Result<()> {
    let cancel = AtomicBool::new(false);
    let progress = |p: &docgait_core::ScanProgress| {
        eprint!("\r{}: {} files, {} folders   ", p.phase, p.files, p.folders);
        let _ = std::io::stderr().flush();
    };
    let summaries = match path {
        Some(p) => {
            let s = lib.add_source(&p, role, level)?;
            vec![scan::scan_source(lib, s.id, &progress, &cancel)?]
        }
        None => {
            if lib.sources()?.is_empty() {
                bail!("no sources yet: run `docgait scan <folder>`");
            }
            scan::scan_all(lib, &progress, &cancel)?
        }
    };
    eprintln!();
    for s in summaries {
        let root = lib.source(s.source_id)?.root;
        println!(
            "{root}: {} files ({}), {} folders, {} gone, {:.1}s",
            s.files,
            human(s.bytes as i64),
            s.folders,
            s.gone,
            s.millis as f64 / 1000.0
        );
        if s.errors > 0 {
            println!("  {} errors, e.g.:", s.errors);
            for e in s.error_samples.iter().take(5) {
                println!("    {e}");
            }
        }
    }
    Ok(())
}

fn sources(lib: &Library, cmd: Option<SourcesCmd>) -> Result<()> {
    match cmd.unwrap_or(SourcesCmd::List { json: false }) {
        SourcesCmd::List { json } => {
            let list = lib.sources()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            if list.is_empty() {
                println!("no sources yet: run `docgait scan <folder>`");
            }
            for s in list {
                println!(
                    "{:>3}  {}  [{}, {}{}{}]  {} files, {}{}",
                    s.id,
                    s.root,
                    s.role.as_str(),
                    s.index_level.as_str(),
                    if s.removable { ", removable" } else { "" },
                    if s.online { "" } else { ", offline" },
                    s.file_count,
                    human(s.total_size),
                    if s.missing_count > 0 {
                        format!(", {} missing", s.missing_count)
                    } else {
                        String::new()
                    }
                );
            }
            Ok(())
        }
        SourcesCmd::Remove { id } => {
            lib.remove_source(id)?;
            println!("removed source {id} (files untouched)");
            Ok(())
        }
        SourcesCmd::Set {
            id,
            level,
            removable,
            ignore_files,
            excludes,
            includes,
        } => {
            let s = lib.source(id)?;
            if let Some(l) = level {
                lib.set_source_level(id, l.into())?;
            }
            if let Some(r) = removable {
                lib.set_source_removable(id, matches!(r, OnOff::On))?;
            }
            if let Some(i) = ignore_files {
                lib.set_honor_ignore_files(id, matches!(i, OnOff::On))?;
            }
            if excludes.is_some() || includes.is_some() {
                lib.set_source_patterns(
                    id,
                    &excludes.unwrap_or(s.excludes),
                    &includes.unwrap_or(s.includes),
                )?;
            }
            println!("updated source {id}; run `docgait scan` to apply");
            Ok(())
        }
    }
}

fn ls(lib: &Library, path: Option<PathBuf>, sort: SortArg, limit: usize, json: bool) -> Result<()> {
    let Some(path) = path else {
        return sources(lib, Some(SourcesCmd::List { json }));
    };
    let folder = lib.folder_for_path(&path)?;
    let sort = match sort {
        SortArg::Name => SortBy::Name,
        SortArg::Modified => SortBy::Modified,
        SortArg::Size => SortBy::Size,
    };
    let listing = lib.list(folder.id, sort, None, limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&listing)?);
        return Ok(());
    }
    for d in &listing.folders {
        println!(
            "{:>10}  {}/  ({} files, {} folders)",
            "", d.name, d.file_count, d.folder_count
        );
    }
    for f in &listing.files {
        println!(
            "{:>10}  {}{}  [{}]",
            human(f.size),
            f.name,
            if f.missing { "  (missing)" } else { "" },
            f.kind
        );
    }
    if listing.next.is_some() {
        println!("… more files; use --limit to see more");
    }
    Ok(())
}

fn libraries(app: &App, cmd: Option<&LibrariesCmd>) -> Result<()> {
    match cmd.unwrap_or(&LibrariesCmd::List) {
        LibrariesCmd::List => {
            app.default_library()?;
            for l in app.libraries()? {
                println!(
                    "{:>3}  {}  {}",
                    l.id,
                    l.name,
                    l.root.as_deref().unwrap_or("(no library folder yet)")
                );
            }
        }
        LibrariesCmd::Create { name } => {
            let l = app.create_library(name)?;
            println!("created library {} ({})", l.id, l.name);
        }
        LibrariesCmd::Rename { id, name } => {
            app.rename_library(*id, name)?;
            println!("renamed library {id}");
        }
        LibrariesCmd::Root { id, path } => {
            app.set_library_root(*id, path)?;
            println!("library {id} folder set to {}", path.display());
        }
        LibrariesCmd::Remove { id, yes } => {
            let l = app.library_info(*id)?;
            if !yes {
                println!(
                    "would forget library {} ({}) and delete its index; files are never touched. Add --yes to do it.",
                    l.id, l.name
                );
                return Ok(());
            }
            app.remove_library(*id)?;
            println!("forgot library {} ({})", l.id, l.name);
        }
    }
    Ok(())
}

fn import_cmd(
    lib: &Library,
    folders: Vec<PathBuf>,
    dest: &str,
    layout: LayoutArg,
    yes: bool,
    resume: Option<i64>,
    list: bool,
) -> Result<()> {
    let cancel = AtomicBool::new(false);
    let progress = |p: &ImportProgress| {
        if p.total > 0 {
            eprint!("\r{}: {}/{}   ", p.phase, p.done, p.total);
            let _ = std::io::stderr().flush();
        }
    };
    let plan = match resume {
        Some(run) => lib.import_plan(run)?,
        None => {
            if folders.is_empty() {
                bail!("name at least one folder to import (or --resume <plan>)");
            }
            let ids = folders
                .iter()
                .map(|p| lib.folder_for_path(p).map(|f| f.id))
                .collect::<Result<Vec<_>>>()?;
            let layout = match layout {
                LayoutArg::Merge => Layout::Merge,
                LayoutArg::OwnFolder => Layout::OwnFolder,
            };
            let p = lib.plan_import(&ids, dest, layout, &progress, &cancel)?;
            eprintln!();
            p
        }
    };
    println!("plan {} → {}", plan.run_id, plan.dest_path);
    println!("  {} files looked at", plan.files);
    println!("  {} to copy ({})", plan.to_copy, human(plan.bytes_to_copy));
    println!("  {} already in the library", plan.skipped_in_library);
    println!("  {} duplicates (come in once)", plan.skipped_duplicate);
    println!(
        "  {} conflicts{}",
        plan.conflicts,
        if plan.needs_review > 0 {
            format!(
                " ({} need review: dates cannot be trusted)",
                plan.needs_review
            )
        } else {
            String::new()
        }
    );
    let lib_root = lib.library_source()?.map(|s| s.root).unwrap_or_default();
    let short = |p: &str| {
        p.strip_prefix(&lib_root)
            .map(|r| format!("library{r}"))
            .unwrap_or_else(|| p.to_string())
    };
    let conflicts = lib.import_items(plan.run_id, None, true, 0, 60)?;
    if !conflicts.is_empty() {
        println!("conflicts (all versions are kept, next to each other):");
        let mut last = None;
        for c in &conflicts {
            if c.conflict_id != last {
                println!("  •");
                last = c.conflict_id;
            }
            println!(
                "    {} ← {}  ({})",
                short(c.to_path.as_deref().unwrap_or("")),
                short(&c.from_path),
                c.note.as_deref().unwrap_or("")
            );
        }
    }
    if list {
        for it in lib.import_items(plan.run_id, Some("copy"), false, 0, 100_000)? {
            println!(
                "  copy {} → {}",
                it.from_path,
                it.to_path.unwrap_or_default()
            );
        }
    }
    if !yes {
        println!(
            "dry run: nothing copied. Run `docgait import --resume {} --yes` to do it.",
            plan.run_id
        );
        return Ok(());
    }
    let res = lib.execute_import(plan.run_id, &progress, &cancel)?;
    eprintln!();
    println!(
        "copied {} files ({}), {} renamed to make room, {} failed. Undo with `docgait undo {}`.",
        res.copied,
        human(res.bytes as i64),
        res.renamed,
        res.failed,
        res.batch_id
    );
    for e in res.errors {
        println!("  {e}");
    }
    Ok(())
}

fn file_id_for(lib: &Library, path: &std::path::Path) -> Result<i64> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no folder", path.display()))?;
    let folder = lib.folder_for_path(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("no file name"))?
        .to_string_lossy()
        .to_string();
    let mut after = None;
    loop {
        let page = lib.list(folder.id, SortBy::Name, after.as_ref(), 5000)?;
        if let Some(f) = page.files.iter().find(|f| f.name == name) {
            return Ok(f.id);
        }
        match page.next {
            Some(c) => after = Some(c),
            None => bail!(
                "{} is not indexed (excluded, or scan first)",
                path.display()
            ),
        }
    }
}

fn print_plan(plan: &docgait_core::OrganisePlan) {
    for s in &plan.steps {
        match s.op.as_str() {
            "mkdir" => println!("  new folder {}", s.to.as_deref().unwrap_or("")),
            "trash" => println!("  trash {}", s.from.as_deref().unwrap_or("")),
            _ => println!(
                "  {} → {}",
                s.from.as_deref().unwrap_or(""),
                s.to.as_deref().unwrap_or("")
            ),
        }
    }
    for n in &plan.notes_to_update {
        println!("  update links in {n}");
    }
    for w in &plan.warnings {
        println!("  warning: {w}");
    }
}

fn print_result(r: &docgait_core::OrganiseResult) {
    println!(
        "moved {}, created {}, trashed {}, notes updated {}, failed {}. Undo with `docgait undo {}`.",
        r.moved, r.created, r.trashed, r.notes_updated, r.failed, r.batch_id
    );
    for e in &r.errors {
        println!("  {e}");
    }
}

fn run_ops(lib: &Library, ops: &[docgait_core::OrganiseOp], what: &str, yes: bool) -> Result<()> {
    if !yes {
        print_plan(&lib.plan_organise(ops)?);
        println!("dry run: add --yes to do it.");
        return Ok(());
    }
    let r = lib.organise(ops, what)?;
    print_result(&r);
    Ok(())
}

fn reason_text(reason: &str) -> &'static str {
    match reason {
        "builtin" => "built-in excludes (SCM folders, dependencies, OS junk)",
        "cachedir" => "folders tagged with CACHEDIR.TAG",
        "pattern" => "your exclude patterns",
        "symlink" => "symlinks (not followed)",
        _ => "ignore files (.gitignore, .ignore, .rgignore, .docgaitignore)",
    }
}

fn fmt_time(secs: i64) -> String {
    // Local time without pulling in a date crate: let the OS format it when available.
    let d = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs.max(0) as u64);
    let ago = std::time::SystemTime::now()
        .duration_since(d)
        .map(|x| x.as_secs())
        .unwrap_or(0);
    match ago {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", ago / 60),
        3600..=86_399 => format!("{}h ago", ago / 3600),
        _ => format!("{}d ago", ago / 86_400),
    }
}

fn human(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}
