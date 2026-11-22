//! One library: its own index database, its sources, and read queries for browsing.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::app::LibraryInfo;
use crate::exclude::WalkOptions;
use crate::{db, schema};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The library folder itself.
    Library,
    /// Somewhere to bring files from.
    Import,
    /// Indexed in place, never imported.
    Indexed,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Library => "library",
            Role::Import => "import",
            Role::Indexed => "indexed",
        }
    }
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "library" => Role::Library,
            "import" => Role::Import,
            "indexed" => Role::Indexed,
            _ => bail!("unknown source role {s:?} (library, import or indexed)"),
        })
    }
    /// Default index level for a new source with this role.
    pub fn default_level(self) -> IndexLevel {
        match self {
            Role::Library => IndexLevel::Full,
            Role::Import | Role::Indexed => IndexLevel::Names,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IndexLevel {
    Names,
    Full,
    Agent,
}

impl IndexLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            IndexLevel::Names => "names",
            IndexLevel::Full => "full",
            IndexLevel::Agent => "agent",
        }
    }
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "names" => IndexLevel::Names,
            "full" => IndexLevel::Full,
            "agent" => IndexLevel::Agent,
            _ => bail!("unknown index level {s:?} (names, full or agent)"),
        })
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Source {
    pub id: i64,
    pub root: String,
    pub role: Role,
    pub index_level: IndexLevel,
    pub removable: bool,
    pub honor_ignore_files: bool,
    pub excludes: Vec<String>,
    pub includes: Vec<String>,
    pub last_scan_at: Option<i64>,
    pub file_count: i64,
    pub total_size: i64,
    pub missing_count: i64,
    /// The root folder exists right now.
    pub online: bool,
    /// Folder id of the source root, for browsing.
    pub root_folder_id: Option<i64>,
    /// The last "how much is already in the library" check, if any.
    pub coverage: Option<crate::cleanup::Coverage>,
}

impl Source {
    pub fn walk_options(&self) -> WalkOptions {
        WalkOptions {
            honor_ignore_files: self.honor_ignore_files,
            excludes: self.excludes.clone(),
            includes: self.includes.clone(),
        }
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Folder {
    pub id: i64,
    pub source_id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub rel_path: String,
    pub missing: bool,
    pub file_count: i64,
    pub folder_count: i64,
    pub total_size: i64,
}

#[derive(Serialize, Clone, Debug)]
pub struct FileEntry {
    pub id: i64,
    pub folder_id: i64,
    pub name: String,
    pub ext: String,
    pub kind: String,
    pub size: i64,
    pub mtime_ns: i64,
    pub missing: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum SortBy {
    #[default]
    Name,
    Modified,
    Size,
}

/// One page of a folder's files, plus its subfolders on the first page.
#[derive(Serialize, Clone, Debug)]
pub struct Listing {
    pub folder: Folder,
    /// Root first, this folder last.
    pub breadcrumbs: Vec<Folder>,
    pub folders: Vec<Folder>,
    pub files: Vec<FileEntry>,
    /// Pass back as `after` for the next page; `None` when this was the last page.
    pub next: Option<Cursor>,
}

/// Keyset cursor: the sort value and id of the last row returned.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Cursor {
    pub key: serde_json::Value,
    pub id: i64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Status {
    pub sources: i64,
    pub folders: i64,
    pub files: i64,
    pub bytes: i64,
    pub missing: i64,
}

pub struct Library {
    pub info: LibraryInfo,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    db_path: PathBuf,
    writer: Mutex<Connection>,
}

impl Library {
    pub(crate) fn open(info: LibraryInfo, data_dir: &Path, cache_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        std::fs::create_dir_all(cache_dir)?;
        let db_path = data_dir.join("index.db");
        let writer = db::open_writer(&db_path, schema::LIBRARY)?;
        Ok(Self {
            info,
            data_dir: data_dir.to_path_buf(),
            cache_dir: cache_dir.to_path_buf(),
            db_path,
            writer: Mutex::new(writer),
        })
    }

    /// The single write connection. Hold it only for one short transaction at a time.
    pub(crate) fn writer(&self) -> MutexGuard<'_, Connection> {
        self.writer.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A fresh read-only connection; never waits on the writer.
    pub(crate) fn reader(&self) -> Result<Connection> {
        db::open_reader(&self.db_path)
    }

    // ---- sources ----

    pub fn add_source(&self, path: &Path, role: Role, level: Option<IndexLevel>) -> Result<Source> {
        let root =
            dunce::canonicalize(path).with_context(|| format!("cannot open {}", path.display()))?;
        if !root.is_dir() {
            bail!("{} is not a folder", root.display());
        }
        let root_s = root.to_string_lossy().to_string();
        for s in self.sources()? {
            let other = Path::new(&s.root);
            if root.starts_with(other) || other.starts_with(&root) {
                if other == root {
                    return Ok(s);
                }
                bail!(
                    "{} overlaps the existing source {}; a file must belong to one source",
                    root.display(),
                    s.root
                );
            }
        }
        if role == Role::Library && self.sources()?.iter().any(|s| s.role == Role::Library) {
            bail!("this library already has a library folder");
        }
        let level = level.unwrap_or(role.default_level());
        {
            let conn = self.writer();
            conn.execute(
                "INSERT INTO source (root, role, index_level, added_at) VALUES (?1, ?2, ?3, ?4)",
                params![root_s, role.as_str(), level.as_str(), db::now_secs()],
            )?;
            let id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO folder (source_id, parent_id, name, rel_path) VALUES (?1, NULL, '', '')",
                [id],
            )?;
        }
        self.source_by_root(&root_s)
    }

    pub fn remove_source(&self, id: i64) -> Result<()> {
        let n = self
            .writer()
            .execute("DELETE FROM source WHERE id = ?1", [id])?;
        if n == 0 {
            bail!("no source with id {id}");
        }
        Ok(())
    }

    pub fn set_source_level(&self, id: i64, level: IndexLevel) -> Result<()> {
        self.writer().execute(
            "UPDATE source SET index_level = ?2 WHERE id = ?1",
            params![id, level.as_str()],
        )?;
        Ok(())
    }

    pub fn set_source_removable(&self, id: i64, removable: bool) -> Result<()> {
        self.writer().execute(
            "UPDATE source SET removable = ?2 WHERE id = ?1",
            params![id, removable],
        )?;
        Ok(())
    }

    pub fn set_source_patterns(
        &self,
        id: i64,
        excludes: &[String],
        includes: &[String],
    ) -> Result<()> {
        self.writer().execute(
            "UPDATE source SET excludes = ?2, includes = ?3 WHERE id = ?1",
            params![
                id,
                serde_json::to_string(excludes)?,
                serde_json::to_string(includes)?
            ],
        )?;
        Ok(())
    }

    pub fn set_honor_ignore_files(&self, id: i64, on: bool) -> Result<()> {
        self.writer().execute(
            "UPDATE source SET honor_ignore_files = ?2 WHERE id = ?1",
            params![id, on],
        )?;
        Ok(())
    }

    const SOURCE_SELECT: &'static str = "SELECT s.id, s.root, s.role, s.index_level, s.removable, s.honor_ignore_files,
                s.excludes, s.includes, s.last_scan_at,
                (SELECT COUNT(*) FROM file f WHERE f.source_id = s.id AND f.state = 'present'),
                (SELECT COALESCE(SUM(size), 0) FROM file f WHERE f.source_id = s.id AND f.state = 'present'),
                (SELECT COUNT(*) FROM file f WHERE f.source_id = s.id AND f.state = 'missing'),
                (SELECT id FROM folder d WHERE d.source_id = s.id AND d.rel_path = '')
         FROM source s";

    pub fn sources(&self) -> Result<Vec<Source>> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare(&format!("{} ORDER BY s.id", Self::SOURCE_SELECT))?;
        let rows = stmt
            .query_map([], row_to_source)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut rows = rows;
        for s in &mut rows {
            s.coverage = self.coverage(s.id)?;
        }
        Ok(rows)
    }

    pub fn source(&self, id: i64) -> Result<Source> {
        let mut s = self
            .reader()?
            .query_row(
                &format!("{} WHERE s.id = ?1", Self::SOURCE_SELECT),
                [id],
                row_to_source,
            )
            .optional()?
            .with_context(|| format!("no source with id {id}"))?;
        s.coverage = self.coverage(id)?;
        Ok(s)
    }

    fn source_by_root(&self, root: &str) -> Result<Source> {
        self.reader()?
            .query_row(
                &format!("{} WHERE s.root = ?1", Self::SOURCE_SELECT),
                [root],
                row_to_source,
            )
            .optional()?
            .with_context(|| format!("no source at {root}"))
    }

    // ---- browsing ----

    pub fn folder(&self, id: i64) -> Result<Folder> {
        folder_by_id(&self.reader()?, id)
    }

    /// The folder for an absolute path inside one of the sources.
    pub fn folder_for_path(&self, path: &Path) -> Result<Folder> {
        let path = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        for s in self.sources()? {
            if let Ok(rel) = path.strip_prefix(&s.root) {
                let rel = rel_string(rel);
                let conn = self.reader()?;
                return conn
                    .query_row(
                        &format!("{FOLDER_SELECT} WHERE source_id = ?1 AND rel_path = ?2"),
                        params![s.id, rel],
                        row_to_folder,
                    )
                    .optional()?
                    .with_context(|| {
                        format!(
                            "{} is not indexed (excluded, or not scanned yet)",
                            path.display()
                        )
                    });
            }
        }
        bail!("{} is not inside any source", path.display())
    }

    /// Absolute path of a folder.
    pub fn folder_path(&self, id: i64) -> Result<PathBuf> {
        let conn = self.reader()?;
        let (root, rel): (String, String) = conn.query_row(
            "SELECT s.root, d.rel_path FROM folder d JOIN source s ON s.id = d.source_id WHERE d.id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(join_rel(Path::new(&root), &rel))
    }

    /// Absolute path of a file.
    pub fn file_path(&self, id: i64) -> Result<PathBuf> {
        let conn = self.reader()?;
        let (root, rel, name): (String, String, String) = conn.query_row(
            "SELECT s.root, d.rel_path, f.name FROM file f
               JOIN folder d ON d.id = f.folder_id JOIN source s ON s.id = f.source_id
              WHERE f.id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(join_rel(Path::new(&root), &rel).join(name))
    }

    /// One page of a folder: subfolders (first page only) and files, keyset-paged.
    pub fn list(
        &self,
        folder_id: i64,
        sort: SortBy,
        after: Option<&Cursor>,
        limit: usize,
    ) -> Result<Listing> {
        let conn = self.reader()?;
        let folder = folder_by_id(&conn, folder_id)?;

        let mut breadcrumbs = vec![folder.clone()];
        let mut parent = folder.parent_id;
        while let Some(p) = parent {
            let f = folder_by_id(&conn, p)?;
            parent = f.parent_id;
            breadcrumbs.push(f);
        }
        breadcrumbs.reverse();

        let folders = if after.is_none() {
            let mut stmt = conn.prepare(&format!(
                "{FOLDER_SELECT} WHERE parent_id = ?1 ORDER BY name COLLATE NOCASE, id"
            ))?;
            let rows = stmt
                .query_map([folder_id], row_to_folder)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        } else {
            Vec::new()
        };

        let limit = limit.clamp(1, 5000);
        let (col, key_sql) = match sort {
            SortBy::Name => ("name COLLATE NOCASE", "name"),
            SortBy::Modified => ("mtime_ns", "mtime_ns"),
            SortBy::Size => ("size", "size"),
        };
        let base = "SELECT id, folder_id, name, ext, kind, size, mtime_ns, state FROM file WHERE folder_id = ?1";
        let mut files: Vec<FileEntry> = match after {
            None => {
                let mut stmt = conn.prepare(&format!("{base} ORDER BY {col}, id LIMIT ?2"))?;
                let rows = stmt
                    .query_map(params![folder_id, limit as i64 + 1], row_to_file)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            }
            Some(c) => {
                let key = sql_value(&c.key);
                let mut stmt = conn.prepare(&format!(
                    "{base} AND ({col} > ?2 OR ({col} = ?2 AND id > ?3)) ORDER BY {col}, id LIMIT ?4"
                ))?;
                let rows = stmt
                    .query_map(params![folder_id, key, c.id, limit as i64 + 1], row_to_file)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            }
        };
        let next = if files.len() > limit {
            files.truncate(limit);
            files.last().map(|f| Cursor {
                key: match key_sql {
                    "name" => serde_json::Value::from(f.name.clone()),
                    "mtime_ns" => serde_json::Value::from(f.mtime_ns),
                    _ => serde_json::Value::from(f.size),
                },
                id: f.id,
            })
        } else {
            None
        };
        Ok(Listing {
            folder,
            breadcrumbs,
            folders,
            files,
            next,
        })
    }

    pub fn status(&self) -> Result<Status> {
        let conn = self.reader()?;
        Ok(conn.query_row(
            "SELECT (SELECT COUNT(*) FROM source),
                    (SELECT COUNT(*) FROM folder WHERE state = 'present'),
                    (SELECT COUNT(*) FROM file WHERE state = 'present'),
                    (SELECT COALESCE(SUM(size), 0) FROM file WHERE state = 'present'),
                    (SELECT COUNT(*) FROM file WHERE state = 'missing')",
            [],
            |r| {
                Ok(Status {
                    sources: r.get(0)?,
                    folders: r.get(1)?,
                    files: r.get(2)?,
                    bytes: r.get(3)?,
                    missing: r.get(4)?,
                })
            },
        )?)
    }
}

/// `rel_path` uses '/' on every OS.
pub(crate) fn rel_string(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn join_rel(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in rel.split('/').filter(|s| !s.is_empty()) {
        p.push(part);
    }
    p
}

const FOLDER_SELECT: &str =
    "SELECT id, source_id, parent_id, name, rel_path, state, file_count, folder_count, total_size FROM folder";

fn folder_by_id(conn: &Connection, id: i64) -> Result<Folder> {
    conn.query_row(
        &format!("{FOLDER_SELECT} WHERE id = ?1"),
        [id],
        row_to_folder,
    )
    .optional()?
    .with_context(|| format!("no folder with id {id}"))
}

fn row_to_folder(r: &rusqlite::Row) -> rusqlite::Result<Folder> {
    Ok(Folder {
        id: r.get(0)?,
        source_id: r.get(1)?,
        parent_id: r.get(2)?,
        name: r.get(3)?,
        rel_path: r.get(4)?,
        missing: r.get::<_, String>(5)? == "missing",
        file_count: r.get(6)?,
        folder_count: r.get(7)?,
        total_size: r.get(8)?,
    })
}

pub(crate) fn row_to_file(r: &rusqlite::Row) -> rusqlite::Result<FileEntry> {
    Ok(FileEntry {
        id: r.get(0)?,
        folder_id: r.get(1)?,
        name: r.get(2)?,
        ext: r.get(3)?,
        kind: r.get(4)?,
        size: r.get(5)?,
        mtime_ns: r.get(6)?,
        missing: r.get::<_, String>(7)? == "missing",
    })
}

fn row_to_source(r: &rusqlite::Row) -> rusqlite::Result<Source> {
    let root: String = r.get(1)?;
    let json_list = |s: String| serde_json::from_str::<Vec<String>>(&s).unwrap_or_default();
    Ok(Source {
        id: r.get(0)?,
        online: Path::new(&root).is_dir(),
        root,
        role: Role::parse(&r.get::<_, String>(2)?).unwrap_or(Role::Import),
        index_level: IndexLevel::parse(&r.get::<_, String>(3)?).unwrap_or(IndexLevel::Names),
        removable: r.get(4)?,
        honor_ignore_files: r.get(5)?,
        excludes: json_list(r.get(6)?),
        includes: json_list(r.get(7)?),
        last_scan_at: r.get(8)?,
        file_count: r.get(9)?,
        total_size: r.get(10)?,
        missing_count: r.get(11)?,
        root_folder_id: r.get(12)?,
        coverage: None,
    })
}

fn sql_value(v: &serde_json::Value) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    match v {
        serde_json::Value::Number(n) => n.as_i64().map(Value::Integer).unwrap_or(Value::Null),
        serde_json::Value::String(s) => Value::Text(s.clone()),
        _ => Value::Null,
    }
}
