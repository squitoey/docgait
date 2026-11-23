# docgait

A personal, local-first document manager: merge scattered copies of your files into one organised
library, remove duplicates, browse and search everything. Nothing leaves your machine.

Status: **M3 in progress** (slice 1: exact duplicates). Libraries, sources, a names-only background scan with sensible
excludes, live updates, filename/path search, and **import into the library**: merge several
copies of your data into one folder without duplicates, with a preview first and undo after,
then **clean up** the copies once everything is verified to be in the library, and find and remove
**exact duplicates** anywhere. The full plan is in
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
make build   # CLI -> target/release/docgait; app -> target/release/bundle/macos/docgait.app (.deb on Linux)
make run     # build and open the app
make dev     # app with hot reload
make test    # Rust tests
make lint    # the CI lint checks
make check   # lint + test
make fmt     # format everything
make demo    # fresh test data in target/demo, then open the app on it
make help    # list targets
```

`make demo` never touches your real data or Trash: the app's data and trash live in
`target/demo` too. Use `target/demo/Library` as the library folder and
`target/demo/drives/CopyA`, `CopyB`, `CopyC` as sources (overlapping copies with a conflict,
duplicates, a vault, and a code project with ignored build output).

### Debian

```sh
sudo apt install build-essential pkg-config libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev \
  libayatana-appindicator3-dev libssl-dev libxdo-dev patchelf file
make build
sudo apt install ./target/release/bundle/deb/docgait_*.deb
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
docgait dupes                               # find exact duplicates everywhere, list the groups
docgait dupes ~/OldLaptop --safe            # only copies the library already has
docgait dedupe --safe --yes                 # trash those (dry run without --yes)
docgait dedupe --hardlink --yes             # or keep every path, store the bytes once
docgait excluded ~/OldLaptop                # what the scan left out, and why
docgait include-anyway ~/OldLaptop/secret.env
docgait conflicts                           # open conflicts and their versions
docgait resolve 4 keep <path>               # keep one version (keep-current, keep, mark)
docgait accept-suggestions --yes            # keep the suggested version everywhere it is clear
docgait mv ~/Lib/Inbox/W2.pdf ~/Lib/Taxes --yes   # move (dry run without --yes)
docgait rename ~/Lib/Taxes "Tax Returns"
docgait mkdir ~/Lib/Receipts
docgait organise ~/Lib/Inbox "{kind}/{year}" --yes  # rule-based organise (preview without --yes)
docgait tag add receipts ~/Lib/Inbox/*.pdf        # then: docgait search tag:receipts
docgait history                             # imports, clean-ups, resolutions, organise steps
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

### Conflicts

The **Conflicts** tab lists every open conflict (sidebar badge shows how many). For each one:
**Keep this one** (the others go to the trash), **Keep all** (conflict files get normal names),
**Merge…** for text files, or **Mark resolved**. **Accept all suggestions** resolves every conflict
with a clear winner in one step. The merge editor shows the current version, the editable result
and the other version side by side; disagreements are marked `<<<<<<<` / `=======` / `>>>>>>>`
with *use left / use right / use both* per block. With three or more versions, the oldest is used
as the common base for a three-way merge, so changes that don't overlap merge on their own.
Conflict files found in the library that docgait didn't create (e.g. after a rebuild) are picked
up by their `.conflict-` marker. Every resolution can be undone.

### Organise

In Browse: **New folder**, **Rename** (F2), **Move to…**, **Move to trash**, and drag files or
folders onto a folder row or the path at the top. ⌘/Ctrl-click and Shift-click select several.
**Organise…** moves every file under a folder into a structure built from a template
(`{kind}/{year}`, `{year}/{month}`, `{ext}`, `{tag}`, …) after a preview. Names are made unique
(`W2 (2).pdf`) instead of overwriting. Moves inside a source update the index in place, so a file
keeps its tags. Everything is one undoable step.

**Tags**: add them in the details panel (several files at once), search with `tag:receipts`, use
`{tag}` in organise templates.

**Obsidian vaults** (folders with `.obsidian/`): moving or renaming a file inside a vault updates
the links to it in the vault's notes (`[[Note]]`, `[[Note#heading|alias]]`, `![[embed]]`, and
relative markdown links), and a moved note's own relative links. Moving something out of a vault,
or trashing it, warns first. Undo restores the notes' text.

### Clean up afterwards

A separate step, never part of the import. **Check against library** reports how much of a source
is already in the library (byte-identical, by hash). **Clean up** moves only those files to the
trash, re-checking each one and its library copy right before, then removes folders it emptied.
Files the library does not have are never touched. An ignore file (`.gitignore` and friends)
stays while its folder still holds excluded files, so they don't suddenly get indexed. Undo
restores everything from the trash.

## Duplicates

**Duplicates** in the sidebar finds byte-identical files across the library and every source.
Only files that share a size are hashed (size → partial → full BLAKE3, cached), so names-only
sources stay cheap. Each group suggests a copy to keep (★): the library copy first, then the
cleanest name (no "copy", "(2)"), the shortest path, the oldest file. Copies on import sources
that the library already has are marked **safe**; **Select them** picks just those.

Selected copies can be moved to the trash or **replaced with hard links** (same drive; every
path stays, the bytes are stored once). A plan that would remove every copy of something is
blocked, every file and the kept copy are re-hashed right before each action, and **Undo** puts
everything back. **Not duplicates** hides a group for good (by content, so it survives moves).
Browse shows ×N next to files that have copies, and lists the copies in the details pane.

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
- Skipped for indexing is not skipped for importing: a vault's `.obsidian/` and a repository's
  `.git` (or `.hg`, `.svn`, `.jj`) are copied whole with their folder, unless the destination
  already has one (never merged), and never at the library root.
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
