import { useEffect, useState } from "react";
import {
  api,
  errorText,
  events,
  humanSize,
  type CleanupResult,
  type Coverage,
  type ExcludedSummary,
  type ImportProgress,
  type Match,
  type Source,
} from "./api";

interface Props {
  source: Source;
  hasLibrary: boolean;
  onChanged: () => void;
  onError: (msg: string) => void;
}

const REASONS: Record<string, string> = {
  builtin: "Built-in excludes (SCM folders, dependencies, OS junk)",
  cachedir: "Folders tagged with CACHEDIR.TAG",
  pattern: "Your exclude patterns",
  symlink: "Symlinks (not followed)",
  "ignore-file": "Ignore files (.gitignore, .ignore, .rgignore, .docgaitignore)",
};

/** Per-source tools: compare with the library, clean up, and see what the scan left out. */
export default function SourceTools({ source, hasLibrary, onChanged, onError }: Props) {
  const [busy, setBusy] = useState<string | null>(null);
  const [progress, setProgress] = useState<ImportProgress | null>(null);
  const [coverage, setCoverage] = useState<Coverage | null>(source.coverage);
  const [missing, setMissing] = useState<Match[] | null>(null);
  const [confirm, setConfirm] = useState(false);
  const [cleaned, setCleaned] = useState<CleanupResult | null>(null);
  const [excluded, setExcluded] = useState<ExcludedSummary | null>(null);
  const folderId = source.root_folder_id;
  const isLibrary = source.role === "library";

  useEffect(() => {
    const sub = events.onImportProgress(setProgress);
    return () => {
      sub.then((un) => un());
    };
  }, []);

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

  const check = async () => {
    if (folderId == null) return;
    const c = await run("Comparing with the library…", () => api.checkCoverage(folderId));
    if (c) {
      setCoverage(c);
      setMissing(null);
      setCleaned(null);
      onChanged();
    }
  };

  const showMissing = async () => {
    if (folderId == null) return;
    const m = await run("Listing…", () => api.coverageFiles(folderId, false, 1000));
    if (m) setMissing(m);
  };

  const clean = async () => {
    if (folderId == null) return;
    setConfirm(false);
    const r = await run("Cleaning up…", () => api.cleanup(folderId));
    if (r) {
      setCleaned(r);
      setCoverage(null);
      onChanged();
    }
  };

  const undo = async (batchId: number) => {
    const r = await run("Undoing…", () => api.undoBatch(batchId));
    if (r) {
      if (r.failed > 0) onError(`Undo left ${r.failed} item(s): ${r.errors[0] ?? ""}`);
      setCleaned(null);
      onChanged();
    }
  };

  const loadExcluded = async () => {
    const ex = await run("Looking for excluded files…", () => api.excluded(source.id));
    if (ex) setExcluded(ex);
  };

  const includeAnyway = async (rel: string, reason: ExcludedSummary["entries"][number]["reason"]) => {
    await run("Bringing it back…", () => api.includeAnyway(source.id, rel, reason));
    onChanged();
    loadExcluded();
  };

  const pct = coverage && coverage.files > 0 ? Math.floor((coverage.in_library * 100) / coverage.files) : 100;

  return (
    <div className="tools">
      {busy && (
        <div className="busy small">
          {busy}
          {progress && progress.total > 0 && ` ${progress.done.toLocaleString()} / ${progress.total.toLocaleString()}`}
          <button className="link" onClick={() => api.cancelImport()}>
            Cancel
          </button>
        </div>
      )}

      {!isLibrary && (
        <section>
          <h4>Compared with the library</h4>
          {!hasLibrary && <p className="muted small">Choose a library folder first (Import into library).</p>}
          {hasLibrary && !coverage && !cleaned && (
            <p className="muted small">
              Not checked yet. Checking hashes files that could be copies, so it can take a while.
            </p>
          )}
          {coverage && (
            <p>
              <strong>{pct}% is already in the library</strong>: {coverage.in_library.toLocaleString()} of{" "}
              {coverage.files.toLocaleString()} files ({humanSize(coverage.bytes_in_library)}).{" "}
              {coverage.not_in_library > 0 && (
                <>
                  {coverage.not_in_library.toLocaleString()} files ({humanSize(coverage.bytes_not_in_library)}) are not.{" "}
                  <button className="link" disabled={!!busy} onClick={showMissing}>
                    Show them
                  </button>
                </>
              )}
            </p>
          )}
          <div className="row">
            <button disabled={!hasLibrary || !!busy || !source.online} onClick={check}>
              {coverage ? "Check again" : "Check against library"}
            </button>
            <button
              disabled={!coverage || coverage.in_library === 0 || !!busy}
              onClick={() => setConfirm(true)}
              title="Trash the files that are verified to be in the library"
            >
              Clean up…
            </button>
          </div>
          {confirm && coverage && (
            <div className="confirm">
              <p>
                Move <strong>{coverage.in_library.toLocaleString()} files</strong> (
                {humanSize(coverage.bytes_in_library)}) from <code>{source.root}</code> to the trash? Each is re-checked
                against its library copy first. Files the library does not have (
                {coverage.not_in_library.toLocaleString()}) are not touched. You can undo this.
              </p>
              <div className="row">
                <button className="primary" onClick={clean}>
                  Move to trash
                </button>
                <button onClick={() => setConfirm(false)}>Cancel</button>
              </div>
            </div>
          )}
          {cleaned && (
            <div className="confirm">
              <p>
                Moved {cleaned.trashed.toLocaleString()} files ({humanSize(cleaned.bytes)}) to the trash
                {cleaned.folders_removed > 0 && ` and removed ${cleaned.folders_removed} empty folders`}.
                {cleaned.skipped > 0 && ` ${cleaned.skipped} skipped (changed since they were checked).`}
                {cleaned.kept_ignore_files > 0 &&
                  ` Kept ${cleaned.kept_ignore_files} ignore file(s) whose folders still hold excluded files.`}
              </p>
              {cleaned.errors.length > 0 && (
                <ul className="filelist small">
                  {cleaned.errors.map((e) => (
                    <li key={e}>{e}</li>
                  ))}
                </ul>
              )}
              <button onClick={() => undo(cleaned.batch_id)}>Undo</button>
            </div>
          )}
          {missing && (
            <ul className="filelist small">
              {missing.map((m) => (
                <li key={m.path}>
                  {m.path} <span className="muted">({humanSize(m.size)})</span>
                </li>
              ))}
              {missing.length >= 1000 && <li className="muted">… and more</li>}
            </ul>
          )}
        </section>
      )}

      <section>
        <h4>Left out of the index</h4>
        {!excluded && (
          <button disabled={!!busy || !source.online} onClick={loadExcluded}>
            Show what was excluded
          </button>
        )}
        {excluded && excluded.entries.length === 0 && <p className="muted small">Nothing was left out.</p>}
        {excluded && excluded.entries.length > 0 && (
          <>
            <ul className="small reasons">
              {Object.entries(excluded.by_reason).map(([r, n]) => (
                <li key={r}>
                  {n.toLocaleString()} · {REASONS[r] ?? r}
                </li>
              ))}
            </ul>
            <table className="list small">
              <tbody>
                {excluded.entries.map((e) => (
                  <tr key={e.rel}>
                    <td className="grow">
                      {e.rel}
                      {e.is_dir ? "/" : ""}
                    </td>
                    <td className="muted">{e.reason}</td>
                    <td className="actions">
                      {e.reason !== "symlink" && (
                        <button
                          className="link"
                          disabled={!!busy}
                          title={
                            e.reason === "ignore-file"
                              ? "Adds a line to .docgaitignore in this source"
                              : "Stops excluding it for this source"
                          }
                          onClick={() => includeAnyway(e.rel, e.reason)}
                        >
                          Index anyway
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            {excluded.truncated && <p className="muted small">Showing the first {excluded.entries.length}.</p>}
          </>
        )}
      </section>
    </div>
  );
}
