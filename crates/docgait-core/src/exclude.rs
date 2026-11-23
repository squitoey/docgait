//! What the walker skips: default excludes (SCM metadata, dependency/build folders, OS and app
//! junk), ignore files (`.gitignore`, `.ignore`, `.rgignore`, `.docgaitignore`), `CACHEDIR.TAG`,
//! and per-source user patterns.

use std::path::{Path, PathBuf};

use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;

/// docgait's own ignore file. Same syntax as `.gitignore`; it takes precedence over every other
/// ignore file, so `!pattern` can bring back something `.gitignore` excludes.
pub const DOCGAIT_IGNORE: &str = ".docgaitignore";

/// Files that decide what else in their folder gets indexed.
const CONTROL_FILES: &[&str] = &[
    ".gitignore",
    ".ignore",
    ".rgignore",
    DOCGAIT_IGNORE,
    "CACHEDIR.TAG",
];

pub fn is_ignore_file(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| CONTROL_FILES.iter().any(|c| n == *c))
}

/// An ignore file whose folder still holds something else. Removing it would change what gets
/// indexed there (e.g. a `build/` folder it hides would suddenly show up), so clean-up and
/// duplicate removal leave it alone.
pub fn ignore_file_in_use(path: &Path) -> bool {
    if !is_ignore_file(path) {
        return false;
    }
    let Some(dir) = path.parent() else {
        return false;
    };
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().any(|e| !is_ignore_file(&e.path())))
        .unwrap_or(false)
}

/// Folder names that are always skipped (matched against the entry's name).
pub const DEFAULT_DIRS: &[&str] = &[
    // SCM metadata. The working files of a repository are still indexed.
    ".git",
    ".hg",
    ".svn",
    ".bzr",
    "_darcs",
    "CVS",
    "RCS",
    "SCCS",
    ".pijul",
    ".jj",
    ".sl",
    ".fossil",
    "_FOSSIL_",
    ".repo",
    // Dependencies and build output.
    "node_modules",
    "bower_components",
    "__pycache__",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".gradle",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".parcel-cache",
    ".terraform",
    "Pods",
    "DerivedData",
    // Editor / app config.
    ".obsidian",
    ".idea",
    ".vscode",
    // App caches.
    ".thumbnails",
    ".picasaoriginals",
    // macOS.
    ".Spotlight-V100",
    ".Trashes",
    ".fseventsd",
    ".TemporaryItems",
    ".DocumentRevisions-V100",
    // Windows.
    "$RECYCLE.BIN",
    "System Volume Information",
    // Linux.
    "lost+found",
];

/// File name globs that are always skipped.
pub const DEFAULT_FILES: &[&str] = &[
    ".DS_Store",
    "._*",
    ".VolumeIcon.icns",
    "Icon\r",
    ".fslckout",
    "Thumbs.db",
    "ehthumbs.db",
    "desktop.ini",
    "pagefile.sys",
    "hiberfil.sys",
    ".directory",
    ".picasa.ini",
    // Temporary and lock files.
    "~$*",
    ".~lock.*#",
    "*.swp",
    "*.swo",
    "*~",
    "*.tmp",
    "*.crdownload",
    "*.part",
    "*.download",
];

/// Paths under the user's home folder that are skipped.
const DEFAULT_HOME_PATHS: &[&str] = &[
    "Library/Caches",
    ".cache",
    ".Trash",
    ".cargo/registry",
    ".m2/repository",
];

/// macOS packages: indexed as one item, never descended into.
pub const PACKAGE_EXTS: &[&str] = &[
    "app",
    "photoslibrary",
    "fcpbundle",
    "imovielibrary",
    "aplibrary",
    "musiclibrary",
    "framework",
    "bundle",
    "xcodeproj",
    "xcworkspace",
];

const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// Per-source walker settings.
#[derive(Clone, Debug, Default)]
pub struct WalkOptions {
    pub honor_ignore_files: bool,
    /// Extra user patterns, gitignore syntax, relative to the source root.
    pub excludes: Vec<String>,
    /// Names or globs that bring back something the *default* excludes would skip.
    pub includes: Vec<String>,
}

/// The default excludes plus `includes`, ready to match entries.
#[derive(Clone)]
pub struct DefaultRules {
    dirs: GlobSet,
    files: GlobSet,
    includes: GlobSet,
    home_paths: Vec<PathBuf>,
}

impl DefaultRules {
    pub fn new(includes: &[String]) -> Result<Self> {
        Ok(Self {
            dirs: globset(DEFAULT_DIRS.iter().copied())?,
            files: globset(DEFAULT_FILES.iter().copied())?,
            includes: globset(includes.iter().map(String::as_str))?,
            home_paths: dirs::home_dir()
                .map(|h| DEFAULT_HOME_PATHS.iter().map(|p| h.join(p)).collect())
                .unwrap_or_default(),
        })
    }

    /// True if the entry at `path` should be skipped by the default rules.
    pub fn excludes(&self, path: &Path, is_dir: bool) -> bool {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        if self.includes.is_match(name) {
            return false;
        }
        if is_dir {
            if self.dirs.is_match(name) || self.home_paths.iter().any(|p| p == path) {
                return true;
            }
            // Only skip these when they really are what the name suggests.
            match name {
                "venv" | ".venv" | "env" => return path.join("pyvenv.cfg").is_file(),
                "target" => {
                    return path
                        .parent()
                        .is_some_and(|p| p.join("Cargo.toml").is_file())
                }
                _ => {}
            }
            is_cache_dir(path)
        } else {
            self.files.is_match(name)
        }
    }
}

/// A folder tagged per the cache directory tagging spec (<https://bford.info/cachedir/>).
pub fn is_cache_dir(dir: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(dir.join("CACHEDIR.TAG")) else {
        return false;
    };
    let mut buf = [0u8; CACHEDIR_SIGNATURE.len()];
    f.read_exact(&mut buf).is_ok() && buf == CACHEDIR_SIGNATURE
}

/// A macOS package, indexed as a single item.
pub fn is_package(path: &Path) -> bool {
    cfg!(target_os = "macos")
        && path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| PACKAGE_EXTS.iter().any(|p| p.eq_ignore_ascii_case(e)))
}

/// A walker over `root` with every exclusion applied. Hidden files are *not* skipped wholesale:
/// dotfiles can matter; the junk ones are in the default rules.
///
/// With `scope`, only that folder (inside `root`) is walked. The walk still starts at `root` and
/// passes through the folders above `scope`, so their ignore files apply exactly as in a full scan.
pub fn walker(root: &Path, opts: &WalkOptions, scope: Option<&Path>) -> Result<WalkBuilder> {
    let rules = DefaultRules::new(&opts.includes)?;
    let mut b = WalkBuilder::new(root);
    b.hidden(false)
        .follow_links(false)
        .parents(false)
        .require_git(false)
        .git_ignore(opts.honor_ignore_files)
        .git_global(opts.honor_ignore_files)
        .git_exclude(opts.honor_ignore_files)
        .ignore(opts.honor_ignore_files);
    if opts.honor_ignore_files {
        // Later names win: docgait's own file has the last word.
        b.add_custom_ignore_filename(".rgignore");
        b.add_custom_ignore_filename(DOCGAIT_IGNORE);
    }
    if !opts.excludes.is_empty() {
        let mut ob = OverrideBuilder::new(root);
        for pat in &opts.excludes {
            // In an override, a leading '!' means "ignore". With only ignore globs, everything
            // else stays included.
            ob.add(&format!("!{}", pat.trim_start_matches('!')))?;
        }
        b.overrides(ob.build()?);
    }
    let root = root.to_path_buf();
    let scope = scope.map(Path::to_path_buf);
    b.filter_entry(move |e| {
        let path = e.path();
        if path == root {
            return true;
        }
        let is_dir = e.file_type().is_some_and(|t| t.is_dir());
        if let Some(scope) = &scope {
            let inside = path.starts_with(scope);
            let above = is_dir && scope.starts_with(path);
            if !inside && !above {
                return false;
            }
        }
        !rules.excludes(path, is_dir)
    });
    Ok(b)
}

fn globset<'a>(pats: impl Iterator<Item = &'a str>) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in pats {
        b.add(Glob::new(p)?);
    }
    Ok(b.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_rules() {
        let r = DefaultRules::new(&[]).unwrap();
        for f in [
            ".DS_Store",
            "._photo.jpg",
            "Thumbs.db",
            "~$report.docx",
            "notes.txt~",
            "x.crdownload",
        ] {
            assert!(r.excludes(Path::new(f), false), "{f} should be excluded");
        }
        for f in ["report.docx", ".bashrc", "photo.jpg"] {
            assert!(!r.excludes(Path::new(f), false), "{f} should be kept");
        }
    }

    #[test]
    fn includes_override_defaults() {
        let r = DefaultRules::new(&[".vscode".to_string()]).unwrap();
        assert!(!r.excludes(Path::new("/x/.vscode"), true));
        assert!(r.excludes(Path::new("/x/.git"), true));
    }
}
