import { useState } from "react";
import { api, errorText, type OrganisePlan, type OrganiseResult } from "./api";

interface Props {
  folderId: number;
  folderName: string;
  onDone: (result: OrganiseResult, label: string) => void;
  onCancel: () => void;
  onError: (msg: string) => void;
}

const PRESETS: [string, string][] = [
  ["{kind}/{year}", "By kind, then year"],
  ["{year}/{month}", "By year and month"],
  ["{ext}", "By file extension"],
  ["{tag}", "By first tag"],
  ["{kind}/{ext}", "By kind, then extension"],
];

/** Rule-based organise: files under this folder → a structure from a template, with a preview. */
export default function OrganiseDialog({ folderId, folderName, onDone, onCancel, onError }: Props) {
  const [template, setTemplate] = useState("{kind}/{year}");
  const [plan, setPlan] = useState<OrganisePlan | null>(null);
  const [busy, setBusy] = useState(false);

  const preview = async () => {
    setBusy(true);
    try {
      setPlan(await api.previewTemplate(folderId, folderId, template));
    } catch (e) {
      onError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const apply = async () => {
    setBusy(true);
    try {
      const r = await api.organiseByTemplate(folderId, folderId, template);
      onDone(r, `Organised ${folderName} by ${template}`);
    } catch (e) {
      onError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const moves = plan?.steps.filter((s) => s.op === "move") ?? [];
  const short = (p: string | null) => (p ?? "").split(/[/\\]/).slice(-4).join("/");

  return (
    <div className="page">
      <header className="page-header">
        <h2>Organise {folderName}</h2>
        <button onClick={onCancel}>Cancel</button>
      </header>
      <section className="card">
        <p className="small">
          Every file under this folder is moved into a structure built from a template (files keep their names). Tokens:{" "}
          <code>{"{year}"}</code> <code>{"{month}"}</code> <code>{"{day}"}</code> (modified date),{" "}
          <code>{"{kind}"}</code>, <code>{"{ext}"}</code>, <code>{"{tag}"}</code>, <code>{"{folder}"}</code>,{" "}
          <code>{"{source}"}</code>.
        </p>
        <div className="row">
          {PRESETS.map(([t, label]) => (
            <button
              key={t}
              className={t === template ? "primary" : ""}
              onClick={() => {
                setTemplate(t);
                setPlan(null);
              }}
              title={t}
            >
              {label}
            </button>
          ))}
        </div>
        <div className="field">
          <span className="caption">Template</span>
          <input
            type="text"
            value={template}
            onChange={(e) => {
              setTemplate(e.target.value);
              setPlan(null);
            }}
          />
        </div>
        <div className="row">
          <button disabled={busy || !template.trim()} onClick={preview}>
            Preview
          </button>
          <button className="primary" disabled={busy || !plan || moves.length === 0} onClick={apply}>
            Move {moves.length.toLocaleString()} files
          </button>
        </div>
      </section>
      {plan && (
        <section className="card">
          {plan.warnings.map((w) => (
            <p key={w} className="note">
              ⚠ {w}
            </p>
          ))}
          {plan.notes_to_update.length > 0 && (
            <p className="small">Links will be updated in {plan.notes_to_update.length} Obsidian note(s).</p>
          )}
          {moves.length === 0 ? (
            <p className="muted">Everything is already in place.</p>
          ) : (
            <ul className="filelist small">
              {moves.slice(0, 500).map((m) => (
                <li key={m.from}>
                  {short(m.from)} <span className="muted">→ {short(m.to)}</span>
                </li>
              ))}
              {moves.length > 500 && <li className="muted">… and {moves.length - 500} more</li>}
            </ul>
          )}
        </section>
      )}
    </div>
  );
}
