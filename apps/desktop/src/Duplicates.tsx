import { useCallback, useEffect, useState } from "react";
import {
  api,
  errorText,
  events,
  formatDate,
  humanSize,
  type DedupeOp,
  type DedupePlan,
  type DedupeResult,
  type DupeFilter,
  type DupePage,
  type ImportProgress,
  type Source,
} from "./api";

interface Props {
  sources: Source[];
  revision: number;
  onChanged: () => void;
  onError: (msg: string) => void;
}

const PAGE = 100;
const KINDS = ["document", "image", "audio", "video", "ebook", "archive", "code", "other"];

const sourceName = (s: Source) => s.root.split(/[/\\]/).filter(Boolean).pop() ?? s.root;

/** Exact duplicates across the library and every source: find, review, trash or hard-link, undo. */
export default function Duplicates({ sources, revision, onChanged, onError }: Props) {
  const [sourceId, setSourceId] = useState<number | null>(null);
  const [kind, setKind] = useState<string>("");
  const [safeOnly, setSafeOnly] = useState(false);
  const [includeIgnored, setIncludeIgnored] = useState(false);
  const [page, setPage] = useState<DupePage | null>(null);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [busy, setBusy] = useState<string | null>(null);
  const [progress, setProgress] = useState<ImportProgress | null>(null);
  const [plan, setPlan] = useState<DedupePlan | null>(null);
  const [result, setResult] = useState<(DedupeResult & { op: DedupeOp }) | null>(null);

  const filter: DupeFilter = {
    source_id: sourceId,
    kind: kind || null,
    safe_only: safeOnly,
    include_ignored: includeIgnored,
  };
  const filterKey = JSON.stringify(filter);

  useEffect(() => {
    const sub = events.onImportProgress(setProgress);
    return () => {
      sub.then((un) => un());
    };
  }, []);

  const load = useCallback(
    async (more = false) => {
      try {
        const f = JSON.parse(filterKey) as DupeFilter;
        const p = await api.duplicateGroups(f, more && page ? page.groups.length : 0, PAGE);
        setPage((old) => (more && old ? { ...p, groups: [...old.groups, ...p.groups] } : p));
      } catch (e) {
        onError(errorText(e));
      }
    },
    [filterKey, page, onError],
  );

  useEffect(() => {
    load(false);
  }, [filterKey, revision]); // eslint-disable-line react-hooks/exhaustive-deps

  // Drop selections that are no longer shown.
  useEffect(() => {
    if (!page) return;
    const shown = new Set(page.groups.flatMap((g) => g.members.map((m) => m.file_id)));
    setSelected((s) => new Set([...s].filter((id) => shown.has(id))));
  }, [page]);

  const run = async <T,>(label: string, f: () => Promise<T>): Promise<T | null> => {
    setBusy(label);
    setProgress(null);
    try {
      return await f();
    } catch (e) {
      onError(errorText(e));
      return null;
    } finally {
      setBusy(null);
    }
  };

  const find = async () => {
    const folderId = sourceId != null ? (sources.find((s) => s.id === sourceId)?.root_folder_id ?? null) : null;
    const r = await run("Finding duplicates…", () => api.findDuplicates(folderId));
    if (r) {
      setResult(null);
      if (r.cancelled) onError("Stopped; the groups so far are shown.");
      onChanged();
      load(false);
    }
  };

  const autoSelect = async (safe: boolean) => {
    const ids = await run("Selecting…", () => api.autoSelectDuplicates(filter, safe));
    if (ids) setSelected(new Set(ids));
  };

  const toggle = (id: number) =>
    setSelected((s) => {
      const n = new Set(s);
      if (n.has(id)) n.delete(id);
      else n.add(id);
      return n;
    });

  const preview = async (op: DedupeOp) => {
    const p = await run("Checking…", () => api.planDedupe([...selected], op));
    if (p) setPlan(p);
  };

  const apply = async () => {
    if (!plan) return;
    const ids = plan.items.filter((i) => i.status === "ok").map((i) => i.file_id);
    const op = plan.op;
    setPlan(null);
    const r = await run(op === "trash" ? "Moving to the trash…" : "Linking…", () => api.dedupe(ids, op));
    if (r) {
      setResult({ ...r, op });
      setSelected(new Set());
      if (r.errors.length > 0) onError(`${r.skipped} skipped: ${r.errors[0]}`);
      onChanged();
    }
  };

  const undo = async (batchId: number) => {
    const r = await run("Undoing…", () => api.undoBatch(batchId));
    if (r) {
      if (r.failed > 0) onError(`Undo left ${r.failed} item(s): ${r.errors[0] ?? ""}`);
      setResult(null);
      onChanged();
    }
  };

  const ignore = async (hash: string, on: boolean) => {
    await run("Saving…", () => api.ignoreDuplicateGroup(hash, on));
    load(false);
  };

  const selBytes = page
    ? page.groups.flatMap((g) => g.members).reduce((n, m) => (selected.has(m.file_id) ? n + m.size : n), 0)
    : 0;
  const name = (id: number) => {
    const s = sources.find((x) => x.id === id);
    return s ? sourceName(s) : "?";
  };

  return (
    <div className="page">
      <header className="page-header">
        <h2>Duplicates</h2>
        <div className="row">
          <select
            value={sourceId ?? ""}
            onChange={(e) => setSourceId(e.target.value ? Number(e.target.value) : null)}
            title="Which files to look at"
          >
            <option value="">Everything</option>
            {sources.map((s) => (
              <option key={s.id} value={s.id}>
                {sourceName(s)}
                {s.role === "library" ? " (library)" : ""}
              </option>
            ))}
          </select>
          <button className="primary" disabled={!!busy} onClick={find}>
            Find duplicates
          </button>
        </div>
      </header>

      {busy && (
        <div className="busy small">
          {busy}
          {progress && progress.total > 0 && ` ${progress.done.toLocaleString()} / ${progress.total.toLocaleString()}`}
          <button className="link" onClick={() => api.cancelImport()}>
            Cancel
          </button>
        </div>
      )}

      {result && (
        <section className="card">
          <p>
            {result.done.toLocaleString()} file(s){" "}
            {result.op === "hardlink" ? "replaced with hard links" : "moved to the trash"}, freeing{" "}
            {humanSize(result.bytes)}
            {result.skipped > 0 && `; ${result.skipped} skipped`}.{" "}
            <button className="link" disabled={!!busy} onClick={() => undo(result.batch_id)}>
              Undo
            </button>
          </p>
        </section>
      )}

      {page && (
        <section className="card">
          <p>
            <strong>
              {page.total_groups.toLocaleString()} group{page.total_groups === 1 ? "" : "s"}
            </strong>{" "}
            · {page.total_files.toLocaleString()} files · {humanSize(page.total_reclaimable)} reclaimable
          </p>
          {page.safe_files > 0 && (
            <p className="small">
              {page.safe_files.toLocaleString()} copies ({humanSize(page.safe_bytes)}) on import sources are already in
              the library and safe to clean up.{" "}
              <button className="link" disabled={!!busy} onClick={() => autoSelect(true)}>
                Select them
              </button>
            </p>
          )}
          {page.total_groups === 0 && (
            <p className="muted small">
              No duplicates found yet. "Find duplicates" hashes only files that share a size, so names-only sources are
              cheap to check.
            </p>
          )}
          <div className="row small">
            <label>
              <input type="checkbox" checked={safeOnly} onChange={(e) => setSafeOnly(e.target.checked)} /> Only safe to
              clean up
            </label>
            <select value={kind} onChange={(e) => setKind(e.target.value)}>
              <option value="">All kinds</option>
              {KINDS.map((k) => (
                <option key={k} value={k}>
                  {k}
                </option>
              ))}
            </select>
            <label>
              <input type="checkbox" checked={includeIgnored} onChange={(e) => setIncludeIgnored(e.target.checked)} />{" "}
              Show "not duplicates"
            </label>
          </div>
        </section>
      )}

      {page && page.total_groups > 0 && (
        <div className="toolbar row">
          <span className="small">
            {selected.size.toLocaleString()} selected ({humanSize(selBytes)})
          </span>
          <button disabled={!!busy} onClick={() => autoSelect(false)}>
            Select all but the kept copy
          </button>
          <button disabled={!!busy || selected.size === 0} onClick={() => setSelected(new Set())}>
            Clear
          </button>
          <button className="primary" disabled={!!busy || selected.size === 0} onClick={() => preview("trash")}>
            Move to trash…
          </button>
          <button
            disabled={!!busy || selected.size === 0}
            onClick={() => preview("hardlink")}
            title="Keep every path, store the bytes once (same drive only)"
          >
            Replace with hard links…
          </button>
        </div>
      )}

      {plan && (
        <section className="card">
          <h3>{plan.op === "trash" ? "Move to the trash" : "Replace with hard links"}</h3>
          <p>
            {plan.ok.toLocaleString()} file(s), {humanSize(plan.bytes)} freed.
            {plan.skipped > 0 && ` ${plan.skipped} will be skipped.`}
          </p>
          {plan.blocked > 0 && (
            <p className="note">
              ⚠ {plan.blocked} file(s) are the last copy of their content. Deselect one copy in each of those groups
              first.
            </p>
          )}
          <ul className="filelist small">
            {plan.items
              .filter((i) => i.status !== "ok")
              .slice(0, 50)
              .map((i) => (
                <li key={i.file_id}>
                  {i.path} <span className="muted">— {i.note}</span>
                </li>
              ))}
          </ul>
          <div className="row">
            <button className="primary" disabled={plan.blocked > 0 || plan.ok === 0} onClick={apply}>
              {plan.op === "trash" ? `Trash ${plan.ok}` : `Link ${plan.ok}`}
            </button>
            <button onClick={() => setPlan(null)}>Cancel</button>
          </div>
        </section>
      )}

      {page?.groups.map((g) => (
        <section key={g.hash} className={`card dupe-group ${g.ignored ? "ignored" : ""}`}>
          <div className="dupe-head small">
            <span>
              <strong>{g.members.length} copies</strong> · {humanSize(g.size)} each · {g.kind}
              {g.reclaimable > 0 && ` · ${humanSize(g.reclaimable)} reclaimable`}
              {g.in_library && <span className="tag">in library</span>}
            </span>
            <button className="link" onClick={() => ignore(g.hash, !g.ignored)}>
              {g.ignored ? "Show again" : "Not duplicates"}
            </button>
          </div>
          <table className="dupe-members">
            <tbody>
              {g.members.map((m) => (
                <tr key={m.file_id} className={selected.has(m.file_id) ? "selected" : ""}>
                  <td>
                    <input
                      type="checkbox"
                      disabled={m.offline}
                      checked={selected.has(m.file_id)}
                      onChange={() => toggle(m.file_id)}
                      aria-label={`Select ${m.path}`}
                    />
                  </td>
                  <td className="grow wrap" title={m.path}>
                    {m.keeper && <span title="Suggested copy to keep">★ </span>}
                    {m.path}
                  </td>
                  <td className="muted small">{name(m.source_id)}</td>
                  <td className="small">
                    {m.safe && <span className="tag ok">safe</span>}
                    {m.hardlinked && <span className="tag">linked</span>}
                    {m.offline && <span className="tag warn">offline</span>}
                  </td>
                  <td className="num muted small">{formatDate(m.mtime_ns)}</td>
                  <td>
                    <button className="link" disabled={m.offline} onClick={() => api.reveal(m.file_id)}>
                      Show
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </section>
      ))}
      {page && page.groups.length < page.total_groups && (
        <button className="more" onClick={() => load(true)}>
          Show more groups
        </button>
      )}
    </div>
  );
}
