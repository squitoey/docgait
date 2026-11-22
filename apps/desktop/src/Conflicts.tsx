import { useCallback, useEffect, useState } from "react";
import {
  api,
  errorText,
  formatDate,
  humanSize,
  type ConflictVariant,
  type ConflictView,
  type MergeStart,
  type Resolution,
} from "./api";

interface Props {
  revision: number;
  onChanged: () => void;
  onError: (msg: string) => void;
}

const REASON: Record<string, string> = {
  clear_newest: "The newest version keeps the name.",
  broken_variant: "One version is empty; the healthy one keeps the name.",
  dates_unreliable: "The dates can't be trusted (seconds or exact hours apart), so nothing was picked: please review.",
  found_on_disk: "Conflict files found in the library; please review.",
};

/** The Conflicts tab: work through same-path, different-content files like an IDE merge. */
export default function Conflicts({ revision, onChanged, onError }: Props) {
  const [list, setList] = useState<ConflictView[] | null>(null);
  const [selected, setSelected] = useState<number | null>(null);
  const [filter, setFilter] = useState<"all" | "needs_review" | "suggested">("all");
  const [merge, setMerge] = useState<{ conflict: ConflictView; variant: ConflictVariant; start: MergeStart } | null>(
    null,
  );
  const [lastBatch, setLastBatch] = useState<{ id: number; label: string } | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(() => {
    api
      .conflicts()
      .then((l) => {
        setList(l);
        setSelected((s) => (s != null && l.some((c) => c.id === s) ? s : (l[0]?.id ?? null)));
      })
      .catch((e) => onError(errorText(e)));
  }, [onError]);

  useEffect(() => {
    load();
  }, [load, revision]);

  const shown = (list ?? []).filter((c) => filter === "all" || c.state === filter);
  const current = shown.find((c) => c.id === selected) ?? null;

  // Keyboard: ↑/↓ (or k/j) move through the list.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (merge || (e.target as HTMLElement)?.tagName === "INPUT" || (e.target as HTMLElement)?.tagName === "TEXTAREA")
        return;
      const i = shown.findIndex((c) => c.id === selected);
      if ((e.key === "ArrowDown" || e.key === "j") && i < shown.length - 1) {
        setSelected(shown[i + 1].id);
        e.preventDefault();
      } else if ((e.key === "ArrowUp" || e.key === "k") && i > 0) {
        setSelected(shown[i - 1].id);
        e.preventDefault();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [shown, selected, merge]);

  const resolve = async (c: ConflictView, r: Resolution, label: string) => {
    setBusy(true);
    try {
      const batch = await api.resolveConflict(c.id, r);
      setLastBatch({ id: batch, label: `${c.name}: ${label}` });
      setMerge(null);
      onChanged();
      load();
    } catch (e) {
      onError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const acceptAll = async () => {
    setBusy(true);
    try {
      const [batch, n] = await api.acceptSuggestions();
      setLastBatch({ id: batch, label: `kept the suggested version of ${n} conflicts` });
      onChanged();
      load();
    } catch (e) {
      onError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const undo = async () => {
    if (!lastBatch) return;
    try {
      await api.undoBatch(lastBatch.id);
      setLastBatch(null);
      onChanged();
      load();
    } catch (e) {
      onError(errorText(e));
    }
  };

  const openMerge = async (c: ConflictView, v: ConflictVariant) => {
    try {
      const start = await api.mergeStart(c.id, v.path);
      setMerge({ conflict: c, variant: v, start });
    } catch (e) {
      onError(errorText(e));
    }
  };

  const keepAll = (c: ConflictView) => {
    const names: [string, string][] = [];
    for (const v of c.variants) {
      if (v.is_current || !v.exists) continue;
      const base = v.path.split(/[/\\]/).pop() ?? "";
      const suggested = base.replace(/\.conflict-([^.]*?)-(\d{4}-\d{2}-\d{2})(-\d+)?/, " (from $1, $2)");
      const name = window.prompt(`New name for ${base}`, suggested);
      if (name == null) return;
      names.push([v.path, name]);
    }
    resolve(c, { type: "keep-all", names }, "kept all versions");
  };

  if (merge) {
    return (
      <MergeEditor
        conflict={merge.conflict}
        start={merge.start}
        busy={busy}
        onSave={(text) => resolve(merge.conflict, { type: "merge", text }, "merged")}
        onCancel={() => setMerge(null)}
      />
    );
  }

  const suggestedCount = (list ?? []).filter((c) => c.state === "suggested").length;

  return (
    <div className="conflicts-page">
      <div className="page">
        <header className="page-header">
          <h2>Conflicts</h2>
          <div className="row">
            <select value={filter} onChange={(e) => setFilter(e.target.value as typeof filter)} aria-label="Filter">
              <option value="all">All open ({list?.length ?? 0})</option>
              <option value="needs_review">Needs review</option>
              <option value="suggested">Suggested</option>
            </select>
            <button disabled={busy || suggestedCount === 0} onClick={acceptAll}>
              Accept all suggestions ({suggestedCount})
            </button>
          </div>
        </header>
        {lastBatch && (
          <p className="note">
            Done: {lastBatch.label}.{" "}
            <button className="link" onClick={undo}>
              Undo
            </button>
          </p>
        )}
        {list && list.length === 0 && (
          <p className="empty">
            No conflicts. When an import finds the same path with different contents, it shows up here.
          </p>
        )}
        <table className="list files">
          <tbody>
            {shown.map((c) => (
              <tr key={c.id} className={c.id === selected ? "selected" : ""} onClick={() => setSelected(c.id)}>
                <td className="grow">
                  <strong>{c.name}</strong> <span className="muted small">{c.folder || "(library root)"}</span>
                </td>
                <td className="muted small">{c.variants.length} versions</td>
                <td>
                  <span className={c.state === "needs_review" ? "tag warn" : "tag"}>
                    {c.state === "needs_review" ? "needs review" : "suggested"}
                  </span>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        <p className="muted small">↑/↓ or j/k to move through the list.</p>
      </div>

      <aside className="details wide">
        {current ? (
          <>
            <h3 className="wrap">{current.name}</h3>
            <p className="muted small wrap">{current.path}</p>
            <p className="small">{REASON[current.reason] ?? ""}</p>
            {current.variants.map((v) => (
              <div key={v.path} className={v.is_current ? "variant current" : "variant"}>
                <div className="row">
                  <strong className="grow">
                    {v.origin}
                    {v.is_current ? " — has the name now" : ""}
                  </strong>
                  {!v.exists && <span className="tag">missing</span>}
                  {!v.healthy && <span className="tag warn">empty</span>}
                </div>
                <div className="small muted">
                  {humanSize(v.size)} · modified {formatDate(v.mtime_ns)}
                </div>
                <div className="small muted wrap">{v.path.split(/[/\\]/).pop()}</div>
                <div className="row">
                  <button
                    disabled={!v.exists}
                    onClick={() => api.openConflictFile(v.path).catch((e) => onError(errorText(e)))}
                  >
                    Open
                  </button>
                  <button
                    disabled={busy || !v.exists}
                    onClick={() =>
                      resolve(
                        current,
                        v.is_current ? { type: "keep-current" } : { type: "keep-variant", path: v.path },
                        `kept ${v.origin}`,
                      )
                    }
                  >
                    Keep this one
                  </button>
                  {!v.is_current && v.text && current.variants.some((x) => x.is_current && x.text) && (
                    <button disabled={busy} onClick={() => openMerge(current, v)}>
                      Merge…
                    </button>
                  )}
                </div>
              </div>
            ))}
            <div className="row">
              <button disabled={busy} onClick={() => keepAll(current)}>
                Keep all (rename)…
              </button>
              <button disabled={busy} onClick={() => resolve(current, { type: "mark-resolved" }, "marked resolved")}>
                Mark resolved
              </button>
            </div>
            <p className="muted small">Versions you don&apos;t keep go to the trash. Every choice can be undone.</p>
          </>
        ) : (
          <p className="muted">Select a conflict.</p>
        )}
      </aside>
    </div>
  );
}

const BLOCK =
  /<<<<<<< [^\n]*\n([\s\S]*?)(?:\|\|\|\|\|\|\| [^\n]*\n[\s\S]*?)?=======\n([\s\S]*?)>>>>>>> [^\n]*(?:\n|$)/g;

interface MergeProps {
  conflict: ConflictView;
  start: MergeStart;
  busy: boolean;
  onSave: (text: string) => void;
  onCancel: () => void;
}

/** Left (current) | editable result | right (other version), with per-block choices. */
function MergeEditor({ conflict, start, busy, onSave, onCancel }: MergeProps) {
  const [text, setText] = useState(start.result);
  const blocks = [...text.matchAll(BLOCK)];

  const choose = (index: number, pick: "left" | "right" | "both") => {
    // Computed from the current text (not an updater function, which may run twice).
    let n = -1;
    setText(
      text.replace(BLOCK, (whole, left: string, right: string) => {
        n++;
        if (n !== index) return whole;
        return pick === "left" ? left : pick === "right" ? right : left + right;
      }),
    );
  };
  const chooseAll = (pick: "left" | "right") =>
    setText((t) => t.replace(BLOCK, (_w, left: string, right: string) => (pick === "left" ? left : right)));

  return (
    <div className="merge">
      <header className="page-header">
        <div>
          <h2>Merge {conflict.name}</h2>
          <span className="muted small">
            {start.base_label ? `Three-way merge using ${start.base_label} as the common base. ` : ""}
            {blocks.length === 0 ? "No disagreements left." : `${blocks.length} disagreement(s) left.`}
          </span>
        </div>
        <div className="row">
          {blocks.length > 0 && (
            <>
              <button onClick={() => chooseAll("left")}>Use left for all</button>
              <button onClick={() => chooseAll("right")}>Use right for all</button>
            </>
          )}
          <button className="primary" disabled={busy || blocks.length > 0} onClick={() => onSave(text)}>
            Save merge
          </button>
          <button onClick={onCancel}>Cancel</button>
        </div>
      </header>
      {blocks.length > 0 && (
        <div className="blocks">
          {blocks.map((b, i) => (
            <div key={i} className="block small">
              <span className="muted">Disagreement {i + 1}:</span>
              <button className="link" onClick={() => choose(i, "left")}>
                use left
              </button>
              <button className="link" onClick={() => choose(i, "right")}>
                use right
              </button>
              <button className="link" onClick={() => choose(i, "both")}>
                use both
              </button>
              <code className="preview">{(b[1] || "∅").slice(0, 60)}</code>
              <span className="muted">vs</span>
              <code className="preview">{(b[2] || "∅").slice(0, 60)}</code>
            </div>
          ))}
        </div>
      )}
      <div className="panes">
        <div className="pane">
          <div className="caption">{start.left_label}</div>
          <pre>{start.left}</pre>
        </div>
        <div className="pane result">
          <div className="caption">Result (editable)</div>
          <textarea value={text} onChange={(e) => setText(e.target.value)} spellCheck={false} />
        </div>
        <div className="pane">
          <div className="caption">{start.right_label}</div>
          <pre>{start.right}</pre>
        </div>
      </div>
    </div>
  );
}
