//! Tags on files. Kept with the file's index row, so they follow it through in-app moves.

use anyhow::{bail, Result};
use rusqlite::params;
use serde::Serialize;

use crate::Library;

#[derive(Serialize, Clone, Debug)]
pub struct TagCount {
    pub name: String,
    pub files: i64,
}

impl Library {
    /// Every tag with how many present files carry it.
    pub fn tags(&self) -> Result<Vec<TagCount>> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare(
            "SELECT t.name, COUNT(f.id) FROM tag t
               LEFT JOIN file_tag ft ON ft.tag_id = t.id
               LEFT JOIN file f ON f.id = ft.file_id AND f.state = 'present'
              GROUP BY t.id HAVING COUNT(f.id) > 0 ORDER BY t.name COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(TagCount {
                    name: r.get(0)?,
                    files: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn file_tags(&self, file_id: i64) -> Result<Vec<String>> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare(
            "SELECT t.name FROM file_tag ft JOIN tag t ON t.id = ft.tag_id WHERE ft.file_id = ?1
              ORDER BY t.name COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map([file_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }

    pub fn add_tag(&self, file_ids: &[i64], tag: &str) -> Result<()> {
        let tag = tag.trim();
        if tag.is_empty() || tag.contains(char::is_whitespace) && tag.len() > 64 {
            bail!("{tag:?} is not a valid tag");
        }
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        tx.execute("INSERT OR IGNORE INTO tag (name) VALUES (?1)", [tag])?;
        let id: i64 = tx.query_row("SELECT id FROM tag WHERE name = ?1", [tag], |r| r.get(0))?;
        for f in file_ids {
            tx.execute(
                "INSERT OR IGNORE INTO file_tag (file_id, tag_id) VALUES (?1, ?2)",
                params![f, id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_tag(&self, file_ids: &[i64], tag: &str) -> Result<()> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        for f in file_ids {
            tx.execute(
                "DELETE FROM file_tag WHERE file_id = ?1 AND tag_id = (SELECT id FROM tag WHERE name = ?2)",
                params![f, tag],
            )?;
        }
        tx.execute(
            "DELETE FROM tag WHERE id NOT IN (SELECT DISTINCT tag_id FROM file_tag)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }
}
