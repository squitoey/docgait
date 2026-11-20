import { useEffect, useState } from "react";
import { api, errorText, formatDate, humanSize, type SearchResults } from "./api";

interface Props {
  query: string;
  revision: number;
  onShowInFolder: (folderId: number, fileId: number) => void;
  onError: (msg: string) => void;
}

const HELP: [string, string][] = [
  ["lease renewal", "all words, in the name or folder path"],
  ['"lease renewal"', "exact phrase"],
  ["-draft", "exclude"],
  ["type:pdf  kind:image", "extension or kind"],
  ["in:Taxes", "under a folder"],
  ["after:2024-01  before:2024-06", "modified date"],
  ["created-after:2023", "created date"],
  ["size:>10mb", "size"],
];

/** Results for the query typed in the sidebar search box. */
export default function Search({ query, revision, onShowInFolder, onError }: Props) {
  const [results, setResults] = useState<SearchResults | null>(null);
  const [selected, setSelected] = useState<number | null>(null);

  useEffect(() => {
    let live = true;
    // Debounce typing a little.
    const t = setTimeout(() => {
      api
        .search(query, 300)
        .then((r) => live && setResults(r))
        .catch((e) => live && onError(errorText(e)));
    }, 150);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [query, revision, onError]);

  const run = (f: () => Promise<void>) => () => f().catch((e) => onError(errorText(e)));

  if (!query.trim()) {
    return (
      <div className="page">
        <h2>Search</h2>
        <p className="muted">Searches file names and folder paths. Document contents come in a later milestone.</p>
        <table className="list help">
          <tbody>
            {HELP.map(([ex, what]) => (
              <tr key={ex}>
                <td>
                  <code>{ex}</code>
                </td>
                <td className="grow muted">{what}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    );
  }

  return (
    <div className="page">
      <header className="page-header">
        <h2>Search</h2>
        <span className="muted">
          {results ? `${results.hits.length.toLocaleString()}${results.more ? "+" : ""} results` : "Searching…"}
        </span>
      </header>
      {results && results.unsupported.length > 0 && <p className="note">Ignored: {results.unsupported.join(", ")}</p>}
      {results && results.hits.length === 0 && <p className="empty">Nothing matches.</p>}
      {results && results.hits.length > 0 && (
        <table className="list files">
          <thead>
            <tr>
              <th className="grow">Name</th>
              <th>Folder</th>
              <th className="num">Size</th>
              <th className="num">Modified</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {results.hits.map((h) => (
              <tr
                key={h.id}
                className={selected === h.id ? "selected" : ""}
                onClick={() => setSelected(h.id)}
                onDoubleClick={run(() => api.openFile(h.id))}
              >
                <td className="grow">{h.name}</td>
                <td className="muted path" title={`${h.source_root}/${h.folder_path}`}>
                  {h.folder_path || "(source root)"}
                </td>
                <td className="num">{humanSize(h.size)}</td>
                <td className="num">{formatDate(h.mtime_ns)}</td>
                <td className="actions">
                  <button className="link" onClick={() => onShowInFolder(h.folder_id, h.id)}>
                    Show in folder
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {results?.more && (
        <p className="muted">Showing the first {results.hits.length}; narrow the search to see more.</p>
      )}
    </div>
  );
}
