//! SQLite plumbing: connection setup and forward-only numbered migrations.
//!
//! Every database runs in WAL mode. Readers open their own connections, so a long write (a scan
//! batch) never blocks a UI query; writes go through one connection per database.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

/// Opens a read-write connection and applies `migrations`.
pub fn open_writer(path: &Path, migrations: &[&str]) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    configure(&conn)?;
    conn.execute_batch("PRAGMA journal_mode = WAL;")?;
    migrate(&conn, migrations)?;
    Ok(conn)
}

/// Opens a read-only connection. Cheap enough to open one per query.
pub fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("cannot open {}", path.display()))?;
    configure(&conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(())
}

/// Applies every migration after the one recorded in `PRAGMA user_version`, each in its own
/// transaction. Migrations are append-only: never edit one that has shipped.
pub fn migrate(conn: &Connection, migrations: &[&str]) -> Result<()> {
    let current: usize =
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
    if current > migrations.len() {
        anyhow::bail!(
            "this database was created by a newer version of docgait (schema {current}, this build knows {})",
            migrations.len()
        );
    }
    for (i, sql) in migrations.iter().enumerate().skip(current) {
        let version = i + 1;
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)
            .with_context(|| format!("migration {version} failed"))?;
        tx.execute_batch(&format!("PRAGMA user_version = {version}"))?;
        tx.commit()?;
    }
    Ok(())
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_once_and_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let m = ["CREATE TABLE a (x);", "CREATE TABLE b (y);"];
        open_writer(&path, &m[..1]).unwrap();
        let conn = open_writer(&path, &m).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 2);
        // Reopening with the same list is a no-op.
        open_writer(&path, &m).unwrap();
    }

    #[test]
    fn refuses_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        open_writer(&path, &["CREATE TABLE a (x);", "CREATE TABLE b (y);"]).unwrap();
        assert!(open_writer(&path, &["CREATE TABLE a (x);"]).is_err());
    }
}
