//! Database schemas, as forward-only migrations (see `db::migrate`).
//! Append new migrations at the end; never edit one that has shipped.

/// Global `app.db`: the list of libraries and app-wide preferences.
pub const APP: &[&str] = &[
    // 1
    "CREATE TABLE library (
       id            INTEGER PRIMARY KEY,
       name          TEXT NOT NULL UNIQUE,
       root          TEXT,                       -- the library folder; NULL until chosen
       created_at    INTEGER NOT NULL,
       settings_json TEXT NOT NULL DEFAULT '{}'  -- per-library settings (AI policy, defaults, ...)
     );
     CREATE TABLE setting (
       key   TEXT PRIMARY KEY,
       value TEXT NOT NULL
     );",
];

/// Per-library `index.db`.
pub const LIBRARY: &[&str] = &[
    // 1
    "CREATE TABLE source (
       id                 INTEGER PRIMARY KEY,
       root               TEXT NOT NULL UNIQUE,
       role               TEXT NOT NULL DEFAULT 'import',  -- library | import | indexed
       index_level        TEXT NOT NULL DEFAULT 'names',   -- names | full | agent
       faces_enabled      INTEGER NOT NULL DEFAULT 0,
       removable          INTEGER NOT NULL DEFAULT 0,
       honor_ignore_files INTEGER NOT NULL DEFAULT 1,
       excludes           TEXT NOT NULL DEFAULT '[]',      -- JSON: extra user patterns (gitignore syntax)
       includes           TEXT NOT NULL DEFAULT '[]',      -- JSON: 'index anyway' overrides of default excludes
       volume_id          TEXT,
       change_log_pos     INTEGER,                         -- last processed OS change-log position
       scan_gen           INTEGER NOT NULL DEFAULT 0,      -- incremented by every scan
       last_scan_at       INTEGER,
       added_at           INTEGER NOT NULL
     );

     -- Paths are stored once per folder; files store folder_id + name.
     CREATE TABLE folder (
       id          INTEGER PRIMARY KEY,
       source_id   INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
       parent_id   INTEGER REFERENCES folder(id) ON DELETE CASCADE,
       name        TEXT NOT NULL,             -- '' for the source root
       rel_path    TEXT NOT NULL,             -- '' for the root, 'a/b' below it ('/' on every OS)
       index_level TEXT,                      -- NULL = inherit from source
       state       TEXT NOT NULL DEFAULT 'present',  -- present | missing
       seen_gen    INTEGER NOT NULL DEFAULT 0,
       file_count  INTEGER NOT NULL DEFAULT 0,       -- direct children only
       folder_count INTEGER NOT NULL DEFAULT 0,
       total_size  INTEGER NOT NULL DEFAULT 0,
       UNIQUE (source_id, rel_path)
     );
     CREATE INDEX folder_parent ON folder(parent_id, name);

     CREATE TABLE file (
       id           INTEGER PRIMARY KEY,
       source_id    INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
       folder_id    INTEGER NOT NULL REFERENCES folder(id) ON DELETE CASCADE,
       name         TEXT NOT NULL,
       ext          TEXT NOT NULL,            -- lower case, no dot; '' if none
       kind         TEXT NOT NULL,            -- document | image | audio | video | ebook | archive | code | package | other
       size         INTEGER NOT NULL,
       mtime_ns     INTEGER NOT NULL,
       created_ns   INTEGER,
       dev          INTEGER NOT NULL,
       ino          INTEGER NOT NULL,
       state        TEXT NOT NULL DEFAULT 'present',  -- present | missing
       seen_gen     INTEGER NOT NULL DEFAULT 0,
       partial_hash BLOB,
       content_hash BLOB,
       UNIQUE (folder_id, name)
     );
     CREATE INDEX file_folder_mtime ON file(folder_id, mtime_ns);
     CREATE INDEX file_folder_size  ON file(folder_id, size);
     CREATE INDEX file_size         ON file(size);
     CREATE INDEX file_source       ON file(source_id, state);
     CREATE INDEX file_content      ON file(content_hash) WHERE content_hash IS NOT NULL;

     -- User-requested and per-source work. Bulk per-file work is driven by state columns instead.
     CREATE TABLE job (
       id          INTEGER PRIMARY KEY,
       kind        TEXT NOT NULL,
       source_id   INTEGER REFERENCES source(id) ON DELETE CASCADE,
       file_id     INTEGER REFERENCES file(id) ON DELETE CASCADE,
       priority    INTEGER NOT NULL DEFAULT 0,
       state       TEXT NOT NULL DEFAULT 'queued',   -- queued | running | done | failed
       attempts    INTEGER NOT NULL DEFAULT 0,
       error       TEXT,
       not_before  INTEGER,
       created_at  INTEGER NOT NULL
     );
     CREATE UNIQUE INDEX job_unique ON job(kind, source_id, file_id) WHERE state IN ('queued', 'running');

     CREATE TABLE setting (
       key   TEXT PRIMARY KEY,
       value TEXT NOT NULL
     );",
    // 2: filename/path search. Trigram tokens give substring matching ("1234" finds
    // "IMG_1234.jpg"). Contentless: only ids come back; rows are read from `file`.
    // Kept in step with `file` by triggers. Renaming a folder must re-index its files.
    "CREATE VIRTUAL TABLE file_fts USING fts5(
       name, path,
       tokenize = 'trigram remove_diacritics 1',
       content = '', contentless_delete = 1
     );
     CREATE TRIGGER file_fts_insert AFTER INSERT ON file BEGIN
       INSERT INTO file_fts (rowid, name, path)
       VALUES (new.id, new.name, (SELECT rel_path FROM folder WHERE id = new.folder_id));
     END;
     CREATE TRIGGER file_fts_delete AFTER DELETE ON file BEGIN
       DELETE FROM file_fts WHERE rowid = old.id;
     END;
     CREATE TRIGGER file_fts_update AFTER UPDATE OF name, folder_id ON file BEGIN
       DELETE FROM file_fts WHERE rowid = old.id;
       INSERT INTO file_fts (rowid, name, path)
       VALUES (new.id, new.name, (SELECT rel_path FROM folder WHERE id = new.folder_id));
     END;
     INSERT INTO file_fts (rowid, name, path)
       SELECT f.id, f.name, d.rel_path FROM file f JOIN folder d ON d.id = f.folder_id;",
];
