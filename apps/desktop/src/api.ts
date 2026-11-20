// Typed wrappers over the Tauri commands in src-tauri/src/main.rs.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Role = "library" | "import" | "indexed";
export type IndexLevel = "names" | "full" | "agent";
export type SortBy = "name" | "modified" | "size";

export interface LibraryInfo {
  id: number;
  name: string;
  root: string | null;
  created_at: number;
}

export interface AppInfo {
  libraries: LibraryInfo[];
  current: LibraryInfo;
  data_dir: string;
  scanning: boolean;
}

export interface Source {
  id: number;
  root: string;
  role: Role;
  index_level: IndexLevel;
  removable: boolean;
  honor_ignore_files: boolean;
  excludes: string[];
  includes: string[];
  last_scan_at: number | null;
  file_count: number;
  total_size: number;
  missing_count: number;
  online: boolean;
  root_folder_id: number | null;
}

export interface Folder {
  id: number;
  source_id: number;
  parent_id: number | null;
  name: string;
  rel_path: string;
  missing: boolean;
  file_count: number;
  folder_count: number;
  total_size: number;
}

export interface FileEntry {
  id: number;
  folder_id: number;
  name: string;
  ext: string;
  kind: string;
  size: number;
  mtime_ns: number;
  missing: boolean;
}

export interface Cursor {
  key: string | number;
  id: number;
}

export interface Listing {
  folder: Folder;
  breadcrumbs: Folder[];
  folders: Folder[];
  files: FileEntry[];
  next: Cursor | null;
}

export interface Status {
  sources: number;
  folders: number;
  files: number;
  bytes: number;
  missing: number;
}

export interface ScanProgress {
  library_id: number;
  source_id: number;
  phase: string;
  files: number;
  folders: number;
  errors: number;
}

export interface ScanSummary {
  source_id: number;
  files: number;
  folders: number;
  bytes: number;
  gone: number;
  errors: number;
  error_samples: string[];
  cancelled: boolean;
  millis: number;
}

export interface ScanFinished {
  library_id: number;
  summary: ScanSummary | null;
  error: string | null;
}

export const api = {
  appInfo: () => invoke<AppInfo>("app_info"),
  createLibrary: (name: string) => invoke<LibraryInfo>("create_library", { name }),
  renameLibrary: (id: number, name: string) => invoke<void>("rename_library", { id, name }),
  switchLibrary: (id: number) => invoke<LibraryInfo>("switch_library", { id }),
  sources: () => invoke<Source[]>("sources"),
  addSource: (path: string, role: Role, level?: IndexLevel) => invoke<Source>("add_source", { path, role, level }),
  removeSource: (id: number) => invoke<void>("remove_source", { id }),
  setSourceLevel: (id: number, level: IndexLevel) => invoke<void>("set_source_level", { id, level }),
  setSourceRemovable: (id: number, removable: boolean) => invoke<void>("set_source_removable", { id, removable }),
  setHonorIgnoreFiles: (id: number, on: boolean) => invoke<void>("set_honor_ignore_files", { id, on }),
  scan: (sourceId?: number) => invoke<void>("scan", { sourceId }),
  cancelScan: () => invoke<void>("cancel_scan"),
  list: (folderId: number, sort: SortBy, after?: Cursor | null, limit?: number) =>
    invoke<Listing>("list", { folderId, sort, after: after ?? null, limit }),
  status: () => invoke<Status>("status"),
  openFile: (id: number) => invoke<void>("open_file", { id }),
  reveal: (id: number, folder = false) => invoke<void>("reveal", { id, folder }),
};

export const events = {
  onScanProgress: (cb: (p: ScanProgress) => void): Promise<UnlistenFn> =>
    listen<ScanProgress>("scan-progress", (e) => cb(e.payload)),
  onScanFinished: (cb: (p: ScanFinished) => void): Promise<UnlistenFn> =>
    listen<ScanFinished>("scan-finished", (e) => cb(e.payload)),
  onIndexChanged: (cb: (libraryId: number) => void): Promise<UnlistenFn> =>
    listen<{ library_id: number }>("index-changed", (e) => cb(e.payload.library_id)),
};

export function humanSize(bytes: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = bytes;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u++;
  }
  return u === 0 ? `${bytes} B` : `${v.toFixed(1)} ${units[u]}`;
}

export function formatDate(ns: number): string {
  if (!ns) return "";
  return new Date(ns / 1e6).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
}

export function errorText(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e);
}
