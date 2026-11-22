import { Fragment, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, errorText, humanSize, type IndexLevel, type Role, type Source } from "./api";
import SourceTools from "./SourceTools";

interface Props {
  sources: Source[];
  scanning: boolean;
  revision: number;
  onChanged: () => void;
  onBrowse: (folderId: number) => void;
  onError: (msg: string) => void;
}

const ROLE_HELP: Record<Role, string> = {
  library: "The organised home for this library's files.",
  import: "Somewhere to bring files from: old drives, backup copies, Downloads.",
  indexed: "Indexed where it is, never imported.",
};

const LEVEL_HELP: Record<IndexLevel, string> = {
  names: "Names, sizes and dates only. Fast on any drive; never reads file contents.",
  full: "Also text, metadata, thumbnails and duplicate detection (coming in later milestones).",
  agent: "Full, plus AI summaries. Not available until an AI agent is configured.",
};

export default function Sources({ sources, scanning, onChanged, onBrowse, onError }: Props) {
  const [path, setPath] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<number | null>(null);
  const hasLibrary = sources.some((s) => s.role === "library");
  const [role, setRole] = useState<Role>("import");
  const [level, setLevel] = useState<IndexLevel>("names");

  const pick = async () => {
    const chosen = await open({ directory: true, multiple: false, title: "Choose a folder to add" });
    if (typeof chosen === "string") {
      setPath(chosen);
      setLevel(role === "library" ? "full" : "names");
    }
  };

  const add = async () => {
    if (!path) return;
    try {
      if (role === "library") await api.setLibraryFolder(path);
      else await api.addSource(path, role, level);
      setPath(null);
      onChanged();
    } catch (e) {
      onError(errorText(e));
    }
  };

  const act = (f: () => Promise<void>) => async () => {
    try {
      await f();
      onChanged();
    } catch (e) {
      onError(errorText(e));
    }
  };

  return (
    <div className="page">
      <header className="page-header">
        <h2>Sources</h2>
        <div className="row">
          <button disabled={scanning || sources.length === 0} onClick={act(() => api.scan())}>
            Rescan all
          </button>
          <button className="primary" onClick={pick}>
            Add folder…
          </button>
        </div>
      </header>

      {path && (
        <section className="card add-source">
          <div className="path">{path}</div>
          <div className="field">
            <span className="caption">Role</span>
            <div className="choices">
              {(["import", "library", "indexed"] as Role[]).map((r) => (
                <label key={r} className={role === r ? "choice selected" : "choice"}>
                  <input
                    type="radio"
                    name="role"
                    checked={role === r}
                    onChange={() => {
                      setRole(r);
                      setLevel(r === "library" ? "full" : "names");
                    }}
                  />
                  <strong>
                    {r === "library" ? "Library folder" : r === "import" ? "Import source" : "Indexed source"}
                  </strong>
                  <span>{ROLE_HELP[r]}</span>
                </label>
              ))}
            </div>
          </div>
          <div className="field">
            <span className="caption">Index level</span>
            <div className="choices">
              {(["names", "full", "agent"] as IndexLevel[]).map((l) => (
                <label
                  key={l}
                  className={`choice ${level === l ? "selected" : ""} ${l === "agent" ? "disabled" : ""}`}
                  title={l === "agent" ? "Configure an AI agent first (not available yet)" : undefined}
                >
                  <input
                    type="radio"
                    name="level"
                    disabled={l === "agent"}
                    checked={level === l}
                    onChange={() => setLevel(l)}
                  />
                  <strong>{l === "names" ? "Names only" : l === "full" ? "Full" : "Agent"}</strong>
                  <span>{LEVEL_HELP[l]}</span>
                </label>
              ))}
            </div>
          </div>
          <div className="row">
            <button className="primary" onClick={add}>
              Add and scan
            </button>
            <button onClick={() => setPath(null)}>Cancel</button>
          </div>
        </section>
      )}

      {sources.length === 0 && !path && (
        <p className="empty">
          Add a folder to get started. Junk like <code>.git</code>, <code>node_modules</code> and <code>.DS_Store</code>{" "}
          is skipped, and <code>.gitignore</code> files are honoured.
        </p>
      )}

      <table className="list">
        <tbody>
          {sources.map((s) => (
            <Fragment key={s.id}>
              <tr className={s.online ? "" : "offline"}>
                <td className="grow">
                  <button
                    className="link strong"
                    disabled={s.root_folder_id == null}
                    onClick={() => s.root_folder_id != null && onBrowse(s.root_folder_id)}
                  >
                    {s.root}
                  </button>
                  <div className="muted small">
                    {s.role} · {s.index_level === "names" ? "names only" : s.index_level}
                    {s.removable ? " · removable" : ""}
                    {s.honor_ignore_files ? "" : " · ignore files off"}
                    {!s.online ? " · offline" : ""}
                    {s.last_scan_at
                      ? ` · scanned ${new Date(s.last_scan_at * 1000).toLocaleString()}`
                      : " · not scanned yet"}
                  </div>
                </td>
                <td className="num">
                  {s.file_count.toLocaleString()} files
                  <div className="muted small">{humanSize(s.total_size)}</div>
                  {s.missing_count > 0 && <div className="muted small">{s.missing_count.toLocaleString()} missing</div>}
                  {s.coverage && (
                    <div
                      className="small strong"
                      title={`checked ${new Date(s.coverage.checked_at * 1000).toLocaleString()}`}
                    >
                      {s.coverage.files > 0 ? Math.floor((s.coverage.in_library * 100) / s.coverage.files) : 100}% in
                      library
                    </div>
                  )}
                </td>
                <td className="actions">
                  <select
                    value={s.index_level}
                    onChange={(e) => act(() => api.setSourceLevel(s.id, e.target.value as IndexLevel))()}
                    aria-label="Index level"
                  >
                    <option value="names">Names only</option>
                    <option value="full">Full</option>
                    <option value="agent" disabled>
                      Agent
                    </option>
                  </select>
                  <label className="check">
                    <input
                      type="checkbox"
                      checked={s.removable}
                      onChange={(e) => act(() => api.setSourceRemovable(s.id, e.target.checked))()}
                    />
                    Removable
                  </label>
                  <label className="check" title=".gitignore, .ignore, .rgignore, .docgaitignore">
                    <input
                      type="checkbox"
                      checked={s.honor_ignore_files}
                      onChange={(e) => act(() => api.setHonorIgnoreFiles(s.id, e.target.checked))()}
                    />
                    Ignore files
                  </label>
                  <button disabled={scanning || !s.online} onClick={act(() => api.scan(s.id))}>
                    Scan
                  </button>
                  <button disabled={scanning} onClick={act(() => api.removeSource(s.id))} title="Files are not touched">
                    Remove
                  </button>
                  <button className="link" onClick={() => setExpanded(expanded === s.id ? null : s.id)}>
                    {expanded === s.id ? "Less ▴" : "More ▾"}
                  </button>
                </td>
              </tr>
              {expanded === s.id && (
                <tr className="tools-row">
                  <td colSpan={3}>
                    <SourceTools source={s} hasLibrary={hasLibrary} onChanged={onChanged} onError={onError} />
                  </td>
                </tr>
              )}
            </Fragment>
          ))}
        </tbody>
      </table>
    </div>
  );
}
