import { useCallback, useEffect, useState } from "react";
import { api, errorText, events, humanSize, type AppInfo, type ScanProgress, type Source, type Status } from "./api";
import Browse from "./Browse";
import Search from "./Search";
import Import from "./Import";
import Conflicts from "./Conflicts";
import Duplicates from "./Duplicates";
import Sources from "./Sources";
import LibraryPicker from "./LibraryPicker";

type View =
  | { kind: "sources" }
  | { kind: "search" }
  | { kind: "import" }
  | { kind: "conflicts" }
  | { kind: "duplicates" }
  | { kind: "browse"; folderId: number; highlight?: number };

export default function App() {
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [sources, setSources] = useState<Source[]>([]);
  const [status, setStatus] = useState<Status | null>(null);
  const [view, setView] = useState<View>({ kind: "sources" });
  const [progress, setProgress] = useState<ScanProgress | null>(null);
  const [scanning, setScanning] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  // Bumped whenever the index changes so open views reload.
  const [revision, setRevision] = useState(0);
  const [query, setQuery] = useState("");
  const [conflictCount, setConflictCount] = useState(0);

  const refresh = useCallback(async () => {
    try {
      const [i, s, st, cc] = await Promise.all([api.appInfo(), api.sources(), api.status(), api.conflictCount()]);
      setInfo(i);
      setConflictCount(cc);
      setSources(s);
      setStatus(st);
      setScanning(i.scanning);
      if (!i.scanning) setProgress(null);
    } catch (e) {
      setMessage(errorText(e));
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  useEffect(() => {
    const subs = [
      events.onScanProgress((p) => {
        setScanning(true);
        setProgress(p);
      }),
      events.onScanFinished((f) => {
        if (f.error) setMessage(f.error);
        else if (f.summary?.cancelled) setMessage("Scan stopped.");
        else if (f.summary && f.summary.errors > 0)
          setMessage(`Scan finished with ${f.summary.errors} unreadable entries, e.g. ${f.summary.error_samples[0]}`);
      }),
      events.onIndexChanged(() => {
        // Also fired by quiet watcher rescans; `refresh` reads whether a full scan is running.
        setRevision((r) => r + 1);
        refresh();
      }),
    ];
    return () => {
      subs.forEach((p) => p.then((un) => un()));
    };
  }, [refresh]);

  const switchLibrary = async (id: number) => {
    try {
      await api.switchLibrary(id);
      setView({ kind: "sources" });
      setQuery("");
      setRevision((r) => r + 1);
      await refresh();
    } catch (e) {
      setMessage(errorText(e));
    }
  };

  if (!info) {
    return <div className="loading">{message ?? "Opening…"}</div>;
  }

  const sourceName = (s: Source) => s.root.split(/[/\\]/).filter(Boolean).pop() ?? s.root;

  return (
    <div className="app">
      <aside className="side">
        <LibraryPicker info={info} onSwitch={switchLibrary} onChanged={refresh} onError={setMessage} />
        <input
          className="search-box"
          type="search"
          placeholder="Search names and folders"
          value={query}
          onFocus={() => setView({ kind: "search" })}
          onChange={(e) => {
            setQuery(e.target.value);
            setView({ kind: "search" });
          }}
          onKeyDown={(e) => {
            if (e.key === "Escape") setQuery("");
          }}
        />
        <nav>
          <button
            className={view.kind === "sources" ? "nav active" : "nav"}
            onClick={() => setView({ kind: "sources" })}
          >
            Sources
          </button>
          <button className={view.kind === "import" ? "nav active" : "nav"} onClick={() => setView({ kind: "import" })}>
            Import into library
          </button>
          <button
            className={view.kind === "conflicts" ? "nav active" : "nav"}
            onClick={() => setView({ kind: "conflicts" })}
          >
            <span className="nav-label">Conflicts</span>
            {conflictCount > 0 && <span className="badge">{conflictCount}</span>}
          </button>
          <button
            className={view.kind === "duplicates" ? "nav active" : "nav"}
            onClick={() => setView({ kind: "duplicates" })}
          >
            Duplicates
          </button>
          <div className="nav-heading">Browse</div>
          {sources.length === 0 && <div className="nav-empty">No sources yet</div>}
          {sources.map((s) => (
            <button
              key={s.id}
              title={s.root}
              disabled={s.root_folder_id == null}
              className={
                view.kind === "browse" && s.root_folder_id != null && view.folderId === s.root_folder_id
                  ? "nav active"
                  : "nav"
              }
              onClick={() => s.root_folder_id != null && setView({ kind: "browse", folderId: s.root_folder_id })}
            >
              <span className="nav-label">{sourceName(s)}</span>
              {!s.online && <span className="tag">offline</span>}
            </button>
          ))}
        </nav>
      </aside>

      <main className="main">
        {view.kind === "duplicates" ? (
          <Duplicates sources={sources} revision={revision} onChanged={refresh} onError={setMessage} />
        ) : view.kind === "conflicts" ? (
          <Conflicts revision={revision} onChanged={refresh} onError={setMessage} />
        ) : view.kind === "import" ? (
          <Import sources={sources} revision={revision} onChanged={refresh} onError={setMessage} />
        ) : view.kind === "search" ? (
          <Search
            query={query}
            revision={revision}
            onShowInFolder={(folderId, fileId) => setView({ kind: "browse", folderId, highlight: fileId })}
            onError={setMessage}
          />
        ) : view.kind === "sources" ? (
          <Sources
            sources={sources}
            scanning={scanning}
            revision={revision}
            onChanged={refresh}
            onBrowse={(folderId) => setView({ kind: "browse", folderId })}
            onError={setMessage}
          />
        ) : (
          <Browse
            folderId={view.folderId}
            highlight={view.highlight}
            revision={revision}
            onChanged={refresh}
            onNavigate={(folderId) => setView({ kind: "browse", folderId })}
            onError={setMessage}
          />
        )}
      </main>

      <footer className="statusbar">
        <span>
          {status
            ? `${status.files.toLocaleString()} files · ${humanSize(status.bytes)} · ${status.sources} source${status.sources === 1 ? "" : "s"}`
            : ""}
          {status && status.missing > 0 ? ` · ${status.missing.toLocaleString()} missing` : ""}
        </span>
        {scanning && (
          <span className="scan">
            Scanning:{" "}
            {progress
              ? `${progress.files.toLocaleString()} files, ${progress.folders.toLocaleString()} folders`
              : "starting…"}
            <button className="link" onClick={() => api.cancelScan()}>
              Stop
            </button>
          </span>
        )}
        {message && (
          <span className="message" title={message}>
            {message}
            <button className="link" onClick={() => setMessage(null)}>
              Dismiss
            </button>
          </span>
        )}
      </footer>
    </div>
  );
}
