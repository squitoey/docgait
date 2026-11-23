import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  api,
  errorText,
  formatDate,
  humanSize,
  type DupeGroup,
  type FileEntry,
  type Listing,
  type OrganiseOp,
  type OrganiseResult,
  type SortBy,
} from "./api";
import OrganiseDialog from "./OrganiseDialog";

interface Props {
  folderId: number;
  /** A file to select when the folder opens (e.g. from search). */
  highlight?: number;
  revision: number;
  onNavigate: (folderId: number) => void;
  onChanged: () => void;
  onError: (msg: string) => void;
}

const PAGE = 500;
/** Selection keys: "f<id>" for files, "d<id>" for folders. */
type Key = string;

/**
 * One folder: breadcrumbs, subfolders, files (keyset-paged). Select with click, ⌘/Ctrl-click,
 * Shift-click; drag onto a folder or breadcrumb to move. Every change can be undone.
 */
export default function Browse({ folderId, highlight, revision, onNavigate, onChanged, onError }: Props) {
  const [listing, setListing] = useState<Listing | null>(null);
  const [files, setFiles] = useState<FileEntry[]>([]);
  const [sort, setSort] = useState<SortBy>("name");
  const [selected, setSelected] = useState<Set<Key>>(new Set(highlight ? [`f${highlight}`] : []));
  const [anchor, setAnchor] = useState<number | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [done, setDone] = useState<{ result: OrganiseResult; label: string } | null>(null);
  const [organising, setOrganising] = useState(false);
  const [tags, setTags] = useState<string[]>([]);
  const [newTag, setNewTag] = useState("");
  const [dropTarget, setDropTarget] = useState<number | null>(null);
  // Other copies of each listed file (from hashes found so far).
  const [dupes, setDupes] = useState<Record<string, number>>({});
  const [copies, setCopies] = useState<DupeGroup | null>(null);

  // A new folder (or a new highlight) resets the selection; index refreshes keep it.
  useEffect(() => {
    setSelected(new Set(highlight ? [`f${highlight}`] : []));
    setDone(null);
  }, [folderId, highlight]);

  useEffect(() => {
    let live = true;
    api
      .list(folderId, sort, null, PAGE)
      .then((l) => {
        if (!live) return;
        setListing(l);
        setFiles(l.files);
      })
      .catch((e) => live && onError(errorText(e)));
    return () => {
      live = false;
    };
  }, [folderId, sort, revision, onError]);

  useEffect(() => {
    let live = true;
    if (files.length === 0) {
      setDupes({});
      return;
    }
    api
      .duplicateCounts(files.map((f) => f.id))
      .then((d) => live && setDupes(d))
      .catch(() => live && setDupes({}));
    return () => {
      live = false;
    };
  }, [files]);

  const selFiles = files.filter((f) => selected.has(`f${f.id}`));
  const selFolders = (listing?.folders ?? []).filter((d) => selected.has(`d${d.id}`));
  const single = selFiles.length === 1 && selFolders.length === 0 ? selFiles[0] : null;

  useEffect(() => {
    if (!single) {
      setTags([]);
      return;
    }
    api
      .fileTags(single.id)
      .then(setTags)
      .catch((e) => onError(errorText(e)));
  }, [single?.id, revision, onError]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    let live = true;
    setCopies(null);
    if (single && dupes[single.id]) {
      api
        .duplicateGroupOf(single.id)
        .then((g) => live && setCopies(g))
        .catch(() => {});
    }
    return () => {
      live = false;
    };
  }, [single?.id, dupes]); // eslint-disable-line react-hooks/exhaustive-deps

  const more = async () => {
    if (!listing?.next) return;
    setLoadingMore(true);
    try {
      const l = await api.list(folderId, sort, listing.next, PAGE);
      setFiles((f) => [...f, ...l.files]);
      setListing({ ...listing, next: l.next });
    } catch (e) {
      onError(errorText(e));
    } finally {
      setLoadingMore(false);
    }
  };

  const run = (f: () => Promise<void>) => () => f().catch((e) => onError(errorText(e)));

  /** Plans, confirms when there are warnings or note updates, then carries out. */
  const apply = async (ops: OrganiseOp[], label: string) => {
    try {
      const plan = await api.planOrganise(ops);
      if (plan.warnings.length > 0 || plan.notes_to_update.length > 0) {
        const lines = [
          ...plan.warnings.map((w) => `⚠ ${w}`),
          ...(plan.notes_to_update.length > 0
            ? [`Links will be updated in ${plan.notes_to_update.length} Obsidian note(s).`]
            : []),
        ];
        if (!window.confirm(`${label}?\n\n${lines.join("\n")}`)) return;
      }
      const result = await api.organise(ops, label);
      setDone({ result, label });
      setSelected(new Set());
      if (result.errors.length > 0) onError(result.errors[0]);
      onChanged();
    } catch (e) {
      onError(errorText(e));
    }
  };

  const undo = async () => {
    if (!done) return;
    try {
      await api.undoBatch(done.result.batch_id);
      setDone(null);
      onChanged();
    } catch (e) {
      onError(errorText(e));
    }
  };

  const moveTo = async (destFolderId: number, fileIds: number[], folderIds: number[]) => {
    if (fileIds.length + folderIds.length === 0) return;
    await apply(
      [{ type: "move", file_ids: fileIds, folder_ids: folderIds, dest_folder_id: destFolderId }],
      `Move ${fileIds.length + folderIds.length} item(s)`,
    );
  };

  const pickDestination = async () => {
    const here = await api.folderPath(folderId);
    const chosen = await open({ directory: true, multiple: false, title: "Move to folder", defaultPath: here });
    if (typeof chosen !== "string") return;
    try {
      const dest = await api.folderForPath(chosen);
      await moveTo(
        dest,
        selFiles.map((f) => f.id),
        selFolders.map((d) => d.id),
      );
    } catch (e) {
      onError(errorText(e));
    }
  };

  const rename = async () => {
    const item = single ?? (selFolders.length === 1 && selFiles.length === 0 ? selFolders[0] : null);
    if (!item) return;
    const name = window.prompt("New name", item.name);
    if (!name || name === item.name) return;
    const isFile = "kind" in item;
    await apply(
      [{ type: "rename", file_id: isFile ? item.id : null, folder_id: isFile ? null : item.id, name }],
      `Rename ${item.name}`,
    );
  };

  const newFolder = async () => {
    const name = window.prompt("New folder name", "New folder");
    if (!name) return;
    await apply([{ type: "mkdir", parent_folder_id: folderId, name }], `New folder ${name}`);
  };

  const trash = async () => {
    const n = selFiles.length + selFolders.length;
    if (!window.confirm(`Move ${n} item(s) to the trash? You can undo this.`)) return;
    await apply(
      [{ type: "trash", file_ids: selFiles.map((f) => f.id), folder_ids: selFolders.map((d) => d.id) }],
      `Trash ${n} item(s)`,
    );
  };

  const addTag = async () => {
    const tag = newTag.trim();
    if (!tag || selFiles.length === 0) return;
    try {
      await api.addTag(
        selFiles.map((f) => f.id),
        tag,
      );
      setNewTag("");
      if (single) setTags(await api.fileTags(single.id));
      onChanged();
    } catch (e) {
      onError(errorText(e));
    }
  };

  const removeTag = async (tag: string) => {
    if (!single) return;
    try {
      await api.removeTag([single.id], tag);
      setTags(await api.fileTags(single.id));
      onChanged();
    } catch (e) {
      onError(errorText(e));
    }
  };

  // Click / ⌘-click / Shift-click selection over folders then files.
  const order: Key[] = [...(listing?.folders ?? []).map((d) => `d${d.id}`), ...files.map((f) => `f${f.id}`)];
  const click = (key: Key, e: React.MouseEvent) => {
    const idx = order.indexOf(key);
    if (e.shiftKey && anchor != null) {
      const [a, b] = [Math.min(anchor, idx), Math.max(anchor, idx)];
      setSelected(new Set(order.slice(a, b + 1)));
    } else if (e.metaKey || e.ctrlKey) {
      const next = new Set(selected);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      setSelected(next);
      setAnchor(idx);
    } else {
      setSelected(new Set([key]));
      setAnchor(idx);
    }
  };

  // Drag and drop: drag the selection (or the dragged row) onto a folder.
  const dragStart = (key: Key, e: React.DragEvent) => {
    const keys = selected.has(key) ? [...selected] : [key];
    if (!selected.has(key)) setSelected(new Set([key]));
    e.dataTransfer.setData("application/x-docgait", JSON.stringify(keys));
    e.dataTransfer.effectAllowed = "move";
  };
  const dropOn = (destFolderId: number) => (e: React.DragEvent) => {
    e.preventDefault();
    setDropTarget(null);
    const raw = e.dataTransfer.getData("application/x-docgait");
    if (!raw) return;
    const keys: Key[] = JSON.parse(raw);
    const fileIds = keys.filter((k) => k.startsWith("f")).map((k) => Number(k.slice(1)));
    const folderIds = keys
      .filter((k) => k.startsWith("d"))
      .map((k) => Number(k.slice(1)))
      .filter((id) => id !== destFolderId);
    moveTo(destFolderId, fileIds, folderIds);
  };
  const dropProps = (id: number) => ({
    onDragOver: (e: React.DragEvent) => {
      if (e.dataTransfer.types.includes("application/x-docgait")) {
        e.preventDefault();
        setDropTarget(id);
      }
    },
    onDragLeave: () => setDropTarget((t) => (t === id ? null : t)),
    onDrop: dropOn(id),
  });

  if (!listing) return <div className="page muted">Loading…</div>;
  const count = selFiles.length + selFolders.length;

  if (organising) {
    return (
      <OrganiseDialog
        folderId={folderId}
        folderName={listing.folder.name || "(source root)"}
        onDone={(result, label) => {
          setOrganising(false);
          setDone({ result, label });
          onChanged();
        }}
        onCancel={() => setOrganising(false)}
        onError={onError}
      />
    );
  }

  return (
    <div className="browse">
      <div className="page">
        <header className="page-header">
          <nav className="crumbs">
            {listing.breadcrumbs.map((b, i) => (
              <span key={b.id} className={dropTarget === b.id ? "drop" : ""} {...dropProps(b.id)}>
                {i > 0 && <span className="sep">/</span>}
                <button className="link" disabled={b.id === folderId} onClick={() => onNavigate(b.id)}>
                  {b.name || "(source root)"}
                </button>
              </span>
            ))}
          </nav>
          <div className="row">
            <button onClick={newFolder}>New folder</button>
            <button onClick={() => setOrganising(true)}>Organise…</button>
            <button onClick={run(() => api.reveal(folderId, true))}>Show in file manager</button>
          </div>
        </header>

        {count > 0 && (
          <div className="toolbar">
            <span className="muted small">{count} selected</span>
            <button
              disabled={!(count === 1 && (single || selFolders.length === 1))}
              onClick={rename}
              title="Rename (F2)"
            >
              Rename
            </button>
            <button onClick={pickDestination}>Move to…</button>
            <button onClick={trash}>Move to trash</button>
            <button className="link" onClick={() => setSelected(new Set())}>
              Clear
            </button>
          </div>
        )}
        {done && (
          <p className="note">
            {done.label}: {done.result.moved > 0 && `${done.result.moved} moved. `}
            {done.result.created > 0 && `${done.result.created} created. `}
            {done.result.trashed > 0 && `${done.result.trashed} trashed. `}
            {done.result.notes_updated > 0 && `Links updated in ${done.result.notes_updated} note(s). `}
            {done.result.failed > 0 && `${done.result.failed} failed. `}
            <button className="link" onClick={undo}>
              Undo
            </button>
          </p>
        )}

        <table
          className="list files"
          tabIndex={0}
          onKeyDown={(e) => {
            if (e.key === "F2") rename();
            if ((e.key === "Delete" || e.key === "Backspace") && (e.metaKey || e.key === "Delete") && count > 0)
              trash();
          }}
        >
          <thead>
            <tr>
              {(
                [
                  ["name", "Name"],
                  ["size", "Size"],
                  ["modified", "Modified"],
                ] as [SortBy, string][]
              ).map(([key, label]) => (
                <th key={key} className={key === "name" ? "grow" : "num"}>
                  <button className={sort === key ? "link strong" : "link"} onClick={() => setSort(key)}>
                    {label}
                    {sort === key ? " ▲" : ""}
                  </button>
                </th>
              ))}
              <th>Kind</th>
            </tr>
          </thead>
          <tbody>
            {listing.folders.map((d) => (
              <tr
                key={`d${d.id}`}
                draggable
                onDragStart={(e) => dragStart(`d${d.id}`, e)}
                {...dropProps(d.id)}
                className={`folder ${d.missing ? "missing" : ""} ${selected.has(`d${d.id}`) ? "selected" : ""} ${
                  dropTarget === d.id ? "drop" : ""
                }`}
                onClick={(e) => click(`d${d.id}`, e)}
                onDoubleClick={() => onNavigate(d.id)}
              >
                <td className="grow">📁 {d.name}</td>
                <td className="num muted">
                  {d.file_count + d.folder_count > 0
                    ? `${(d.file_count + d.folder_count).toLocaleString()} item${d.file_count + d.folder_count === 1 ? "" : "s"}`
                    : "empty"}
                </td>
                <td className="num" />
                <td className="muted">folder</td>
              </tr>
            ))}
            {files.map((f) => (
              <tr
                key={f.id}
                draggable={!f.missing}
                onDragStart={(e) => dragStart(`f${f.id}`, e)}
                className={`${f.missing ? "missing" : ""} ${selected.has(`f${f.id}`) ? "selected" : ""}`}
                onClick={(e) => click(`f${f.id}`, e)}
                onDoubleClick={run(() => api.openFile(f.id))}
              >
                <td className="grow">
                  {f.name}
                  {dupes[f.id] > 0 && (
                    <span
                      className="tag dupe-badge"
                      title={`${dupes[f.id]} other identical cop${dupes[f.id] === 1 ? "y" : "ies"}`}
                    >
                      ×{dupes[f.id] + 1}
                    </span>
                  )}
                </td>
                <td className="num">{humanSize(f.size)}</td>
                <td className="num">{formatDate(f.mtime_ns)}</td>
                <td className="muted">{f.missing ? "missing" : f.kind}</td>
              </tr>
            ))}
          </tbody>
        </table>
        {listing.folders.length === 0 && files.length === 0 && <p className="empty">This folder is empty.</p>}
        {listing.next && (
          <button className="more" disabled={loadingMore} onClick={more}>
            {loadingMore ? "Loading…" : "Show more"}
          </button>
        )}
        <p className="muted small">
          Double-click a folder to open it. ⌘/Ctrl-click or Shift-click to select several; drag onto a folder or the
          path above to move.
        </p>
      </div>

      <aside className="details">
        {single ? (
          <>
            <h3 className="wrap">{single.name}</h3>
            <dl>
              <dt>Kind</dt>
              <dd>{single.kind}</dd>
              <dt>Size</dt>
              <dd>
                {humanSize(single.size)} ({single.size.toLocaleString()} bytes)
              </dd>
              <dt>Modified</dt>
              <dd>{formatDate(single.mtime_ns)}</dd>
              {single.missing && (
                <>
                  <dt>State</dt>
                  <dd>Missing (not found at the last scan)</dd>
                </>
              )}
            </dl>
            {copies && (
              <div className="copies small">
                <span className="caption">Identical copies</span>
                <ul className="filelist">
                  {copies.members
                    .filter((m) => m.file_id !== single.id)
                    .map((m) => (
                      <li key={m.file_id} title={m.path}>
                        {m.keeper && "★ "}
                        {m.path}
                      </li>
                    ))}
                </ul>
              </div>
            )}
            <div className="row">
              <button disabled={single.missing} onClick={run(() => api.openFile(single.id))}>
                Open
              </button>
              <button disabled={single.missing} onClick={run(() => api.reveal(single.id))}>
                Show in file manager
              </button>
            </div>
          </>
        ) : count > 1 ? (
          <h3>{count} items selected</h3>
        ) : (
          <p className="muted">Select a file to see its details.</p>
        )}
        {selFiles.length > 0 && (
          <div className="tags">
            <span className="caption">Tags</span>
            <div className="row">
              {tags.map((t) => (
                <span key={t} className="tag-chip">
                  {t}
                  <button className="link" onClick={() => removeTag(t)} aria-label={`Remove ${t}`}>
                    ×
                  </button>
                </span>
              ))}
            </div>
            <input
              type="text"
              placeholder={selFiles.length > 1 ? `Add a tag to ${selFiles.length} files` : "Add a tag"}
              value={newTag}
              onChange={(e) => setNewTag(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") addTag();
              }}
            />
          </div>
        )}
      </aside>
    </div>
  );
}
