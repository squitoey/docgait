//! The global `app.db`: which libraries exist, and where their data lives.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::{db, schema, Library};

/// Name of the library created on first run.
pub const DEFAULT_LIBRARY: &str = "Personal";

#[derive(Serialize, Clone, Debug)]
pub struct LibraryInfo {
    pub id: i64,
    pub name: String,
    /// The library folder, once chosen.
    pub root: Option<String>,
    pub created_at: i64,
}

pub struct App {
    data_dir: PathBuf,
    cache_dir: PathBuf,
    conn: Mutex<Connection>,
}

impl App {
    /// Opens the app data in the OS default locations. `DOCGAIT_DATA_DIR` puts everything
    /// (data and caches) under one folder instead, which is handy for experiments and tests.
    pub fn open_default() -> Result<Self> {
        if let Ok(dir) = std::env::var("DOCGAIT_DATA_DIR") {
            return Self::open(Path::new(&dir));
        }
        // macOS: ~/Library/Application Support and ~/Library/Caches.
        // Linux: ~/.local/share and ~/.cache.
        let data = dirs::data_dir()
            .context("cannot find the user data folder")?
            .join("docgait");
        let cache = dirs::cache_dir()
            .context("cannot find the user cache folder")?
            .join("docgait");
        Self::open_with(&data, &cache)
    }

    /// Keeps everything, caches included, under `dir`.
    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_with(dir, &dir.join("cache"))
    }

    pub fn open_with(data_dir: &Path, cache_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        std::fs::create_dir_all(cache_dir)?;
        let conn = db::open_writer(&data_dir.join("app.db"), schema::APP)?;
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            cache_dir: cache_dir.to_path_buf(),
            conn: Mutex::new(conn),
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn libraries(&self) -> Result<Vec<LibraryInfo>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT id, name, root, created_at FROM library ORDER BY id")?;
        let rows = stmt
            .query_map([], row_to_info)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn create_library(&self, name: &str) -> Result<LibraryInfo> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a library needs a name");
        }
        let conn = self.conn();
        conn.execute(
            "INSERT INTO library (name, created_at) VALUES (?1, ?2)",
            params![name, db::now_secs()],
        )
        .with_context(|| format!("cannot create library {name:?} (is the name already used?)"))?;
        let id = conn.last_insert_rowid();
        drop(conn);
        self.library_info(id)
    }

    pub fn library_info(&self, id: i64) -> Result<LibraryInfo> {
        self.conn()
            .query_row(
                "SELECT id, name, root, created_at FROM library WHERE id = ?1",
                [id],
                row_to_info,
            )
            .optional()?
            .with_context(|| format!("no library with id {id}"))
    }

    pub fn find_library(&self, name: &str) -> Result<LibraryInfo> {
        self.conn()
            .query_row(
                "SELECT id, name, root, created_at FROM library WHERE name = ?1 COLLATE NOCASE",
                [name],
                row_to_info,
            )
            .optional()?
            .with_context(|| format!("no library named {name:?}"))
    }

    /// The first library, creating "Personal" on first run.
    pub fn default_library(&self) -> Result<LibraryInfo> {
        match self.libraries()?.into_iter().next() {
            Some(l) => Ok(l),
            None => self.create_library(DEFAULT_LIBRARY),
        }
    }

    pub fn rename_library(&self, id: i64, name: &str) -> Result<()> {
        let n = self.conn().execute(
            "UPDATE library SET name = ?2 WHERE id = ?1",
            params![id, name.trim()],
        )?;
        if n == 0 {
            bail!("no library with id {id}");
        }
        Ok(())
    }

    pub fn set_library_root(&self, id: i64, root: &Path) -> Result<()> {
        let root =
            dunce::canonicalize(root).with_context(|| format!("cannot open {}", root.display()))?;
        if !root.is_dir() {
            bail!("{} is not a folder", root.display());
        }
        self.conn().execute(
            "UPDATE library SET root = ?2 WHERE id = ?1",
            params![id, root.to_string_lossy()],
        )?;
        Ok(())
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT value FROM setting WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO setting (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Opens a library's own index database (created on first use).
    pub fn open_library(&self, id: i64) -> Result<Library> {
        let info = self.library_info(id)?;
        let dir = self.data_dir.join("libraries").join(id.to_string());
        let cache = self.cache_dir.join("libraries").join(id.to_string());
        Library::open(info, &dir, &cache)
    }

    /// Forgets a library: deletes its index and cache, never the user's files.
    pub fn remove_library(&self, id: i64) -> Result<()> {
        self.conn()
            .execute("DELETE FROM library WHERE id = ?1", [id])?;
        for dir in [
            self.data_dir.join("libraries").join(id.to_string()),
            self.cache_dir.join("libraries").join(id.to_string()),
        ] {
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
            }
        }
        Ok(())
    }
}

fn row_to_info(r: &rusqlite::Row) -> rusqlite::Result<LibraryInfo> {
    Ok(LibraryInfo {
        id: r.get(0)?,
        name: r.get(1)?,
        root: r.get(2)?,
        created_at: r.get(3)?,
    })
}
