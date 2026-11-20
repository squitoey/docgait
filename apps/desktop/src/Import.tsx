import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  api,
  errorText,
  events,
  humanSize,
  type BatchInfo,
  type ImportItem,
  type ImportPlan,
  type ImportProgress,
  type ImportResult,
  type Layout,
  type Source,
} from "./api";

interface Props {
  sources: Source[];
  revision: number;
  onChanged: () => void;
  onError: (msg: string) => void;
}

type Stage =
  | { kind: "setup" }
  | { kind: "planning" }
  | { kind: "plan"; plan: ImportPlan }
  | { kind: "copying"; plan: ImportPlan }
  | { kind: "done"; plan: ImportPlan; result: ImportResult };

const sourceName = (s: Source) => s.root.split(/[/\\]/).filter(Boolean).pop() ?? s.root;

/** Merge sources into the library folder: choose → plan (dry run) → import → undo if needed. */
export default function Import({ sources, revision, onChanged, onError }: Props) {
  const library = sources.find((s) => s.role === "library") ?? null;
  const candidates = sources.filter((s) => s.role !== "library");
  const [picked, setPicked] = useState<Set<number>>(new Set());
  const [dest, setDest] = useState("");
  const [layout, setLayout] = useState<Layout>("merge");
  const [stage, setStage] = useState<Stage>({ kind: "setup" });
  const [progress, setProgress] = useState<ImportProgress | null>(null);
  const [history, setHistory] = useState<BatchInfo[]>([]);

  useEffect(() => {
    const sub = events.onImportProgress(setProgress);
    return () => {
      sub.then((un) => un());
    };
  }, []);

  useEffect(() => {
    api
      .history()
      .then(setHistory)
      .catch((e) => onError(errorText(e)));
  }, [revision, stage.kind, onError]);

  const chooseLibrary = async () => {
    const chosen = await open({ directory: true, multiple: false, title: "Choose the library folder" });
    if (typeof chosen !== "string") return;
    try {
      await api.setLibraryFolder(chosen);
      onChanged();
    } catch (e) {
      onError(errorText(e));
    }
  };

  const plan = async () => {
    const folderIds = candidates
      .filter((s) => picked.has(s.id) && s.root_folder_id != null)
      .map((s) => s.root_folder_id as number);
    setProgress(null);
    setStage({ kind: "planning" });
    try {
      const p = await api.planImport(folderIds, dest, layout);
      setStage({ kind: "plan", plan: p });
    } catch (e) {
      onError(errorText(e));
      setStage({ kind: "setup" });
    }
  };

  const run = async (p: ImportPlan) => {
    setProgress(null);
    setStage({ kind: "copying", plan: p });
    try {
      const result = await api.executeImport(p.run_id);
      setStage({ kind: "done", plan: p, result });
      onChanged();
    } catch (e) {
      onError(errorText(e));
      setStage({ kind: "plan", plan: p });
    }
  };

  const undo = async (batchId: number) => {
    try {
      const r = await api.undoBatch(batchId);
      if (r.failed > 0) onError(`Undo left ${r.failed} item(s): ${r.errors[0] ?? ""}`);
      setStage({ kind: "setup" });
      onChanged();
      setHistory(await api.history());
    } catch (e) {
      onError(errorText(e));
    }
  };

  if (!library) {
    return (
      <div className="page">
        <h2>Import into the library</h2>
        <section className="card">
          <p>
            First choose this library&apos;s <strong>library folder</strong>: the one organised place your files will be
            merged into. It can be an empty folder.
          </p>
          <button className="primary" onClick={chooseLibrary}>
            Choose library folder…
          </button>
        </section>
      </div>
    );
  }

  return (
    <div className="page">
      <header className="page-header">
        <h2>Import into the library</h2>
        <span className="muted small" title={library.root}>
          Library folder: {library.root}
        </span>
      </header>

      {stage.kind === "setup" && (
        <section className="card">
          <div className="field">
            <span className="caption">Bring files from</span>
            {candidates.length === 0 && (
              <p className="muted">Add the folders you want to import on the Sources page first.</p>
            )}
            {candidates.map((s) => (
              <label key={s.id} className="check-row">
                <input
                  type="checkbox"
                  disabled={!s.online || s.root_folder_id == null}
                  checked={picked.has(s.id)}
                  onChange={(e) => {
                    const next = new Set(picked);
                    if (e.target.checked) next.add(s.id);
                    else next.delete(s.id);
                    setPicked(next);
                  }}
                />
                <span className="grow" title={s.root}>
                  <strong>{sourceName(s)}</strong> <span className="muted small">{s.root}</span>
                </span>
                <span className="muted small">
                  {s.file_count.toLocaleString()} files · {humanSize(s.total_size)}
                  {!s.online ? " · offline" : ""}
                </span>
              </label>
            ))}
          </div>
          <div className="field">
            <span className="caption">How</span>
            <div className="choices two">
              <label className={layout === "merge" ? "choice selected" : "choice"}>
                <input type="radio" checked={layout === "merge"} onChange={() => setLayout("merge")} />
                <strong>Merge folders</strong>
                <span>For copies of the same data: matching folders are combined into one structure.</span>
              </label>
              <label className={layout === "own-folder" ? "choice selected" : "choice"}>
                <input type="radio" checked={layout === "own-folder"} onChange={() => setLayout("own-folder")} />
                <strong>Keep each in its own folder</strong>
                <span>Each source goes into a folder named after it.</span>
              </label>
            </div>
          </div>
          <div className="field">
            <span className="caption">Into (folder inside the library, optional)</span>
            <input
              type="text"
              placeholder="e.g. Imported/2024 — leave empty for the library root"
              value={dest}
              onChange={(e) => setDest(e.target.value)}
            />
          </div>
          <div className="row">
            <button className="primary" disabled={picked.size === 0} onClick={plan}>
              Preview import
            </button>
            <span className="muted small">Nothing is copied until you confirm. Sources are never changed.</span>
          </div>
        </section>
      )}

      {stage.kind === "planning" && (
        <section className="card">
          <p>Comparing files…</p>
          <ProgressBar progress={progress} />
          <button onClick={() => api.cancelImport()}>Cancel</button>
        </section>
      )}

      {(stage.kind === "plan" || stage.kind === "copying" || stage.kind === "done") && (
        <PlanView
          plan={stage.plan}
          stage={stage}
          progress={progress}
          onRun={() => run(stage.plan)}
          onBack={() => setStage({ kind: "setup" })}
          onUndo={undo}
          onError={onError}
        />
      )}

      {history.length > 0 && (
        <section>
          <h3 className="section-title">History</h3>
          <table className="list">
            <tbody>
              {history.map((b) => (
                <tr key={b.id}>
                  <td className="grow">
                    {b.description}
                    <div className="muted small">
                      {new Date(b.created_at * 1000).toLocaleString()} · {b.state}
                      {b.undone > 0 ? ` · ${b.undone} undone` : ""}
                    </div>
                  </td>
                  <td className="actions">
                    <button disabled={b.state === "undone" || b.state === "running"} onClick={() => undo(b.id)}>
                      Undo
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </section>
      )}
    </div>
  );
}

function ProgressBar({ progress }: { progress: ImportProgress | null }) {
  const pct = progress && progress.total > 0 ? Math.round((progress.done / progress.total) * 100) : 0;
  return (
    <div className="progress">
      <div className="bar" style={{ width: `${pct}%` }} />
      <span className="small muted">
        {progress
          ? `${progress.phase} ${progress.done.toLocaleString()} / ${progress.total.toLocaleString()}${
              progress.bytes_total ? ` · ${humanSize(progress.bytes_done)} of ${humanSize(progress.bytes_total)}` : ""
            }`
          : "starting…"}
      </span>
    </div>
  );
}

interface PlanViewProps {
  plan: ImportPlan;
  stage: Stage;
  progress: ImportProgress | null;
  onRun: () => void;
  onBack: () => void;
  onUndo: (batchId: number) => void;
  onError: (msg: string) => void;
}

function PlanView({ plan, stage, progress, onRun, onBack, onUndo, onError }: PlanViewProps) {
  const [conflicts, setConflicts] = useState<ImportItem[]>([]);
  const [copies, setCopies] = useState<ImportItem[] | null>(null);

  useEffect(() => {
    api
      .importItems(plan.run_id, null, true, 0, 500)
      .then(setConflicts)
      .catch((e) => onError(errorText(e)));
  }, [plan.run_id, onError]);

  const showCopies = () =>
    api
      .importItems(plan.run_id, "copy", false, 0, 1000)
      .then(setCopies)
      .catch((e) => onError(errorText(e)));

  // Group conflict items by conflict id.
  const groups = new Map<number, ImportItem[]>();
  for (const c of conflicts) {
    if (c.conflict_id == null) continue;
    const g = groups.get(c.conflict_id) ?? [];
    g.push(c);
    groups.set(c.conflict_id, g);
  }
  const rel = (p: string | null) => (p && p.startsWith(plan.dest_path) ? p.slice(plan.dest_path.length + 1) : p);

  return (
    <section className="card">
      <div className="stats">
        <Stat label="to copy" value={plan.to_copy.toLocaleString()} sub={humanSize(plan.bytes_to_copy)} strong />
        <Stat label="already in the library" value={plan.skipped_in_library.toLocaleString()} />
        <Stat label="duplicates (come in once)" value={plan.skipped_duplicate.toLocaleString()} />
        <Stat
          label="conflicts"
          value={plan.conflicts.toLocaleString()}
          sub={plan.needs_review > 0 ? `${plan.needs_review} need review` : undefined}
        />
      </div>
      <p className="muted small">
        {plan.files.toLocaleString()} files compared. Destination: {plan.dest_path}
      </p>

      {groups.size > 0 && (
        <details open={groups.size <= 10}>
          <summary>
            Conflicts: the same path with different contents. Every version is kept; the suggested one keeps the name,
            the others sit next to it as <code>.conflict-</code> files.
          </summary>
          <div className="conflicts">
            {[...groups.entries()].map(([id, items]) => (
              <div key={id} className="conflict">
                {items.map((it) => (
                  <div key={it.id} className="small">
                    <span className={it.note?.includes("suggested") ? "strong" : ""}>{rel(it.to_path)}</span>
                    <span className="muted">
                      {" "}
                      ← {it.action === "rename_existing" ? "already in the library" : it.from_path}
                    </span>
                  </div>
                ))}
              </div>
            ))}
          </div>
        </details>
      )}

      {stage.kind === "plan" && (
        <>
          <div className="row">
            <button className="primary" disabled={plan.to_copy === 0} onClick={onRun}>
              Import {plan.to_copy.toLocaleString()} files
            </button>
            <button onClick={onBack}>Back</button>
            <button className="link" onClick={showCopies}>
              List the files to copy
            </button>
          </div>
          {plan.to_copy === 0 && <p className="muted">Everything here is already in the library.</p>}
          {copies && (
            <ul className="filelist small">
              {copies.map((c) => (
                <li key={c.id}>
                  {rel(c.to_path)} <span className="muted">← {c.from_path}</span>
                </li>
              ))}
              {copies.length >= 1000 && <li className="muted">… and more</li>}
            </ul>
          )}
        </>
      )}

      {stage.kind === "copying" && (
        <>
          <p>Copying and verifying…</p>
          <ProgressBar progress={progress} />
          <button onClick={() => api.cancelImport()}>Stop</button>
          <span className="muted small"> Stopping keeps what is done; you can resume later.</span>
        </>
      )}

      {stage.kind === "done" && (
        <>
          <p>
            <strong>
              Copied {stage.result.copied.toLocaleString()} files ({humanSize(stage.result.bytes)}).
            </strong>{" "}
            {stage.result.renamed > 0 && `${stage.result.renamed} library files renamed to make room. `}
            {stage.result.failed > 0 && `${stage.result.failed} failed. `}
            {stage.result.cancelled && "Stopped before the end. "}
          </p>
          {stage.result.errors.length > 0 && (
            <ul className="filelist small">
              {stage.result.errors.map((e) => (
                <li key={e}>{e}</li>
              ))}
            </ul>
          )}
          <div className="row">
            <button onClick={() => onUndo(stage.result.batch_id)}>Undo this import</button>
            {(stage.result.cancelled || stage.result.failed > 0) && <button onClick={onRun}>Resume / retry</button>}
            <button onClick={onBack}>Import more</button>
          </div>
        </>
      )}
    </section>
  );
}

function Stat({ label, value, sub, strong }: { label: string; value: string; sub?: string; strong?: boolean }) {
  return (
    <div className={strong ? "stat strong" : "stat"}>
      <div className="value">{value}</div>
      <div className="label">{label}</div>
      {sub && <div className="sub">{sub}</div>}
    </div>
  );
}
