# docgait

A personal, local-first document manager: merge scattered copies of your files into one organised
library, remove duplicates, browse and search everything. Nothing leaves your machine.

Status: **M0 (bootstrap)**. Libraries, sources, a names-only background scan with sensible
excludes, and a desktop app to browse the result. The full plan is in
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
docgait status
docgait libraries create Work               # libraries are fully separate
docgait -L Work scan ~/WorkDrive
```

Use `--data-dir <dir>` (or `DOCGAIT_DATA_DIR`) to work on a separate index, which is handy for
experiments.

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
