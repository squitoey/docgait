# docgait

A personal, local-first document manager: merge scattered copies of your files into one organised
library, remove duplicates, browse and search everything. Nothing leaves your machine.

Status: **M2, slice 2**. Libraries, sources, a names-only background scan with sensible
excludes, live updates, filename/path search, and **import into the library**: merge several
copies of your data into one folder without duplicates, with a preview first and undo after,
then **clean up** the copies once everything is verified to be in the library. The full plan is in
[plan/docgait-plan.md](plan/docgait-plan.md).

## Layout

| Path | What |
| --- | --- |
| `crates/docgait-core` | All logic: libraries, sources, scanning, excludes, the SQLite index |
| `crates/docgait-cli` | `docgait` command-line tool |
| `apps/desktop` | Tauri 2 + React desktop app |
| `plan/` | The plan |

## Build

Needs Rust, Node and pnpm. If `cargo` is not on your PATH: `. "$HOME/.cargo/env"`.

```sh
cargo build --release -p docgait-cli                  # -> target/release/docgait
cd apps/desktop && pnpm install && pnpm tauri build  # macOS: target/release/bundle/macos/docgait.app
```

During development: `cd apps/desktop && pnpm tauri dev`. Tests: `cargo test --workspace`.

### Debian

```sh
sudo apt install build-essential pkg-config libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev \
  libayatana-appindicator3-dev libssl-dev libxdo-dev patchelf file
cd apps/desktop && pnpm install && pnpm tauri build --bundles deb
sudo apt install ../../target/release/bundle/deb/docgait_*.deb
```

CI builds the `.deb` on `debian:bookworm` on every push (artifact `docgait-deb`).

## CLI

The CLI and the app share the same index.

```sh
docgait scan ~/Documents                    # add the folder as an import source and scan it
docgait scan ~/Library-Folder --role library  # the library folder (full index level by default)
docgait scan                                # rescan every source
docgait sources                             # list sources
docgait sources set 2 --removable on        # keep files that cannot be found (drives that come and go)
docgait sources set 2 --exclude 'private/' --include .vscode
docgait ls ~/Documents/Taxes --sort size    # list a folder from the index
docgait search taxes 2023 type:pdf          # names and folder paths; see below
docgait status
docgait library-folder ~/Library-Folder     # this library's folder (where imports go)
docgait import ~/OldLaptop ~/Backup2019     # preview: what would be copied, skipped, in conflict
docgait import --resume 1 --yes             # do it (or resume an interrupted import)
docgait check ~/OldLaptop --missing         # how much is already in the library; list the rest
docgait cleanup ~/OldLaptop --yes           # trash files verified to be in the library
docgait excluded ~/OldLaptop                # what the scan left out, and why
docgait include-anyway ~/OldLaptop/secret.env
docgait history                             # imports and clean-ups so far
docgait undo 1 --yes                        # undo an import: its copies go to the trash
docgait libraries create Work               # libraries are fully separate
docgait -L Work scan ~/WorkDrive
```

Use `--data-dir <dir>` (or `DOCGAIT_DATA_DIR`) to work on a separate index, which is handy for
experiments.

## Import into the library

Pick sources (or folders inside them) and merge them into the library folder:

- **Merge folders** (default) combines matching paths: `OldLaptop/Taxes/W2.pdf` and
  `Backup2019/Taxes/W2.pdf` both map to `Library/Taxes/W2.pdf`. **Own folder** puts each source in
  a folder named after it.
- Files already anywhere in the library are skipped. The same file appearing several times comes
  in once (the copy with the cleanest name). Files are compared by size, then a partial hash, then
  a full BLAKE3 hash, and only when needed; hashes are cached in the index.
- **Conflicts** (same path, different contents): every version is kept, next to each other. The
  suggested version keeps the name (a healthy file beats an empty one, then the newest), the
  others become `name.conflict-<source>-<date>.ext`. If the dates cannot be trusted (seconds
  apart, an exact number of hours apart, or obviously wrong), the conflict is marked *needs
  review* instead of guessing.
- Nothing is copied until you confirm. Every copy is verified by hash, written under a temporary
  name first, and journaled. An interrupted import resumes; a whole import can be undone (copies
  go to the trash, renames are reversed). Sources are never changed.

### Clean up afterwards

A separate step, never part of the import. **Check against library** reports how much of a source
is already in the library (byte-identical, by hash). **Clean up** moves only those files to the
trash, re-checking each one and its library copy right before, then removes folders it emptied.
Files the library does not have are never touched. Undo restores everything from the trash.

### What was left out

Each source's **More ▾ → Show what was excluded** lists everything the scan skipped and why
(built-in excludes, `CACHEDIR.TAG`, your patterns, symlinks, ignore files), with **Index anyway**.
For files hidden by an ignore file, that adds a `!` line to the source's `.docgaitignore`.

`DOCGAIT_TRASH_DIR=<folder>` replaces the OS trash with a plain folder, for experiments.

## Search

Names and folder paths for now (document contents come later). Every word must match, in the file
name or anywhere in its folder path; matching is by substring, ignoring case and accents.

| Input | Meaning |
| --- | --- |
| `lease renewal` | all words |
| `"lease renewal"` | exact phrase |
| `-draft`, `-"old copy"` | exclude |
| `type:pdf` / `kind:image` | extension / kind |
| `in:Taxes` | under a folder whose path contains the text |
| `after:2024-01 before:2024-06` | modified date (year, month or day) |
| `created-after:2023` | created date |
| `size:>10mb`, `size:<1kb` | size |

## Live updates

While the app is open it watches every source (FSEvents on macOS, inotify on Linux) and rescans
just the folder that changed, a second or so after the change. Changes made while it was closed
are picked up by a rescan when it starts.

## What gets skipped

- SCM metadata (`.git`, `.hg`, `.svn`, `.bzr`, `_darcs`, `CVS`, `.jj`, `.sl`, Fossil, …); the
  working files of a repository are still indexed.
- Dependency and build folders (`node_modules`, Python virtualenvs, `__pycache__`, Rust `target`
  next to `Cargo.toml`, …), editor config (`.obsidian`, `.idea`, `.vscode`).
- OS and app junk (`.DS_Store`, `._*`, `Thumbs.db`, `desktop.ini`, Office/LibreOffice lock files,
  partial downloads, …) and folders tagged with `CACHEDIR.TAG`.
- Anything matched by `.gitignore`, `.ignore`, `.rgignore` or `.docgaitignore`. `.docgaitignore`
  wins over the others, so `!notes.txt` there brings back a file `.gitignore` excludes.
- macOS packages (`.app`, `.photoslibrary`, …) are indexed as one item.

Per source: extra excludes, "index anyway" includes, and ignore files on/off.

## Where data lives

| Data | macOS | Debian |
| --- | --- | --- |
| Library list (`app.db`) | `~/Library/Application Support/docgait/` | `~/.local/share/docgait/` |
| Each library's index | `…/docgait/libraries/<id>/index.db` | same, under `~/.local/share` |
| Caches | `~/Library/Caches/docgait/` | `~/.cache/docgait/` |

Nothing is written into your folders. Deleting these locations resets the app.
