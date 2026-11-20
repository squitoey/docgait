//! `docgait` command-line tool: a thin layer over docgait-core. Shares the index with the app.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use docgait_core::{scan, App, IndexLevel, Library, Role, SortBy};

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
