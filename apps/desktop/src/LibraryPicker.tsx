import { useState } from "react";
import { api, errorText, type AppInfo } from "./api";

interface Props {
  info: AppInfo;
  onSwitch: (id: number) => void;
  onChanged: () => void;
  onError: (msg: string) => void;
}

/** Library switcher at the top of the sidebar, with "New library…". */
export default function LibraryPicker({ info, onSwitch, onChanged, onError }: Props) {
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");

  const create = async () => {
    try {
      const lib = await api.createLibrary(name);
      setCreating(false);
      setName("");
      onChanged();
      onSwitch(lib.id);
    } catch (e) {
      onError(errorText(e));
    }
  };

  if (creating) {
    return (
      <div className="library-picker">
        <input
          autoFocus
          placeholder="Library name (e.g. Work)"
          value={name}
          onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && name.trim()) create();
            if (e.key === "Escape") setCreating(false);
          }}
        />
        <div className="row">
          <button className="primary" disabled={!name.trim()} onClick={create}>
            Create
          </button>
          <button onClick={() => setCreating(false)}>Cancel</button>
        </div>
      </div>
    );
  }

  return (
    <div className="library-picker">
      <label className="caption" htmlFor="library">
        Library
      </label>
      <select
        id="library"
        value={info.current.id}
        onChange={(e) => {
          if (e.target.value === "new") setCreating(true);
          else onSwitch(Number(e.target.value));
        }}
      >
        {info.libraries.map((l) => (
          <option key={l.id} value={l.id}>
            {l.name}
          </option>
        ))}
        <option value="new">New library…</option>
      </select>
    </div>
  );
}
