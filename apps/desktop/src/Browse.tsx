import { useEffect, useState } from "react";
import { api, errorText, formatDate, humanSize, type FileEntry, type Listing, type SortBy } from "./api";

interface Props {
  folderId: number;
  revision: number;
  onNavigate: (folderId: number) => void;
  onError: (msg: string) => void;
}

const PAGE = 500;

/** One folder: breadcrumbs, subfolders, then files (keyset-paged, "Show more"). */
export default function Browse({ folderId, revision, onNavigate, onError }: Props) {
  const [listing, setListing] = useState<Listing | null>(null);
  const [files, setFiles] = useState<FileEntry[]>([]);
  const [sort, setSort] = useState<SortBy>("name");
  const [selected, setSelected] = useState<number | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);

  useEffect(() => {
    let live = true;
    setSelected(null);
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

  if (!listing) return <div className="page muted">Loading…</div>;
  const sel = files.find((f) => f.id === selected) ?? null;

  return (
    <div className="browse">
      <div className="page">
        <header className="page-header">
          <nav className="crumbs">
            {listing.breadcrumbs.map((b, i) => (
              <span key={b.id}>
                {i > 0 && <span className="sep">/</span>}
                <button className="link" disabled={b.id === folderId} onClick={() => onNavigate(b.id)}>
                  {b.name || "(source root)"}
                </button>
              </span>
            ))}
          </nav>
          <div className="row">
            <button onClick={run(() => api.reveal(folderId, true))}>Show in file manager</button>
          </div>
        </header>

        <table className="list files">
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
                className={d.missing ? "folder missing" : "folder"}
                onDoubleClick={() => onNavigate(d.id)}
              >
                <td className="grow">
                  <button className="link" onClick={() => onNavigate(d.id)}>
                    📁 {d.name}
                  </button>
                </td>
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
                className={`${f.missing ? "missing" : ""} ${selected === f.id ? "selected" : ""}`}
                onClick={() => setSelected(f.id)}
                onDoubleClick={run(() => api.openFile(f.id))}
              >
                <td className="grow">{f.name}</td>
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
      </div>

      <aside className="details">
        {sel ? (
          <>
            <h3 className="wrap">{sel.name}</h3>
            <dl>
              <dt>Kind</dt>
              <dd>{sel.kind}</dd>
              <dt>Size</dt>
              <dd>
                {humanSize(sel.size)} ({sel.size.toLocaleString()} bytes)
              </dd>
              <dt>Modified</dt>
              <dd>{formatDate(sel.mtime_ns)}</dd>
              {sel.missing && (
                <>
                  <dt>State</dt>
                  <dd>Missing (not found at the last scan)</dd>
                </>
              )}
            </dl>
            <div className="row">
              <button disabled={sel.missing} onClick={run(() => api.openFile(sel.id))}>
                Open
              </button>
              <button disabled={sel.missing} onClick={run(() => api.reveal(sel.id))}>
                Show in file manager
              </button>
            </div>
          </>
        ) : (
          <p className="muted">Select a file to see its details.</p>
        )}
      </aside>
    </div>
  );
}
