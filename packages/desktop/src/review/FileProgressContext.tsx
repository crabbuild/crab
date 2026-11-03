/**
 * FileProgressContext — tracks which files the reviewer has marked as "viewed"
 * in a PR, persisted to localStorage keyed by repo + PR number.
 *
 * Staleness detection: when the PR head SHA changes, files whose patch content
 * has changed are automatically reset to unviewed.
 *
 * Usage:
 *   <FileProgressProvider repoId={repo.repo_id} prNumber={prNumber} files={changedFiles} headSha={headSha}>
 *     …
 *   </FileProgressProvider>
 *
 *   const { markViewed, markUnviewed, isViewed, viewedCount, totalFiles } = useFileProgress();
 */

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";

import type { FileProgressEntry, FileProgressState } from "./types";
import type { PrChangedFile } from "../ipc";
import { publishToast } from "../lib/toasts";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function storageKey(repoId: string, prNumber: number): string {
  return `crab.fileProgress.${repoId}.${prNumber}`;
}

/**
 * FNV-1a 32-bit hash — fast, non-cryptographic hash suitable for detecting
 * patch content changes. Returns a hex string.
 */
function fnv1aHash(input: string): string {
  let hash = 0x811c9dc5; // FNV offset basis
  for (let i = 0; i < input.length; i++) {
    hash ^= input.charCodeAt(i);
    // FNV prime: multiply by 16777619 using bit math to stay in 32-bit range
    hash = Math.imul(hash, 0x01000193);
  }
  // Convert to unsigned 32-bit then hex
  return (hash >>> 0).toString(16).padStart(8, "0");
}

function loadFromStorage(repoId: string, prNumber: number): FileProgressState | null {
  try {
    const raw = localStorage.getItem(storageKey(repoId, prNumber));
    if (!raw) return null;
    const parsed: FileProgressState = JSON.parse(raw);
    if (!parsed || !Array.isArray(parsed.files)) return null;
    return parsed;
  } catch {
    return null;
  }
}

function persistToStorage(repoId: string, prNumber: number, state: FileProgressState): void {
  try {
    localStorage.setItem(storageKey(repoId, prNumber), JSON.stringify(state));
  } catch (err: unknown) {
    const isQuota =
      err instanceof DOMException &&
      (err.name === "QuotaExceededError" || err.code === 22);

    publishToast({
      kind: "error",
      title: "Failed to save file progress",
      body: isQuota
        ? "Local storage is full. File progress is safe in memory but may be lost on restart."
        : "Could not persist file progress to local storage.",
    });
  }
}

// ---------------------------------------------------------------------------
// Context types
// ---------------------------------------------------------------------------

export interface FileProgressContextValue {
  markViewed(path: string): void;
  markUnviewed(path: string): void;
  isViewed(path: string): boolean;
  viewedCount: number;
  totalFiles: number;
  resetStaleFiles(newHeadSha: string, changedFiles: PrChangedFile[]): number;
}

const FileProgressContext = createContext<FileProgressContextValue | null>(null);

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

interface FileProgressProviderProps {
  repoId: string;
  prNumber: number;
  /** The list of changed files in the PR (with patch content for hashing). */
  files: PrChangedFile[];
  /** Current head SHA of the PR branch. */
  headSha: string;
  children: ReactNode;
}

export function FileProgressProvider({
  repoId,
  prNumber,
  files,
  headSha,
  children,
}: FileProgressProviderProps) {
  // Map of path → FileProgressEntry for O(1) lookups.
  const [entries, setEntries] = useState<Map<string, FileProgressEntry>>(() => {
    const stored = loadFromStorage(repoId, prNumber);
    if (!stored) return new Map();
    const map = new Map<string, FileProgressEntry>();
    for (const entry of stored.files) {
      map.set(entry.path, entry);
    }
    return map;
  });

  // Keep a ref for use in callbacks without stale closures.
  const entriesRef = useRef(entries);
  entriesRef.current = entries;

  // Build a lookup from path → patch content for hashing.
  const filesByPath = useRef<Map<string, PrChangedFile>>(new Map());
  useEffect(() => {
    const map = new Map<string, PrChangedFile>();
    for (const f of files) {
      map.set(f.path, f);
    }
    filesByPath.current = map;
  }, [files]);

  // Re-load from storage if repoId/prNumber changes.
  useEffect(() => {
    const stored = loadFromStorage(repoId, prNumber);
    if (!stored) {
      setEntries(new Map());
      return;
    }
    const map = new Map<string, FileProgressEntry>();
    for (const entry of stored.files) {
      map.set(entry.path, entry);
    }
    setEntries(map);
  }, [repoId, prNumber]);

  // Persist helper — writes current entries to localStorage.
  const persist = useCallback(
    (currentEntries: Map<string, FileProgressEntry>) => {
      const state: FileProgressState = {
        prNumber,
        headSha,
        files: Array.from(currentEntries.values()),
      };
      persistToStorage(repoId, prNumber, state);
    },
    [repoId, prNumber, headSha],
  );

  const markViewed = useCallback(
    (path: string) => {
      setEntries((prev) => {
        const next = new Map(prev);
        const file = filesByPath.current.get(path);
        const patchHash = file?.patch ? fnv1aHash(file.patch) : "";
        const entry: FileProgressEntry = {
          path,
          viewedAtSha: headSha,
          patchHash,
        };
        next.set(path, entry);
        persist(next);
        return next;
      });
    },
    [headSha, persist],
  );

  const markUnviewed = useCallback(
    (path: string) => {
      setEntries((prev) => {
        const next = new Map(prev);
        next.delete(path);
        persist(next);
        return next;
      });
    },
    [persist],
  );

  const isViewed = useCallback(
    (path: string): boolean => {
      return entriesRef.current.has(path);
    },
    [],
  );

  const resetStaleFiles = useCallback(
    (newHeadSha: string, changedFiles: PrChangedFile[]): number => {
      let resetCount = 0;
      setEntries((prev) => {
        const next = new Map(prev);
        for (const file of changedFiles) {
          const entry = next.get(file.path);
          if (!entry) continue;
          const currentPatchHash = file.patch ? fnv1aHash(file.patch) : "";
          if (entry.patchHash !== currentPatchHash) {
            next.delete(file.path);
            resetCount++;
          }
        }
        const state: FileProgressState = {
          prNumber,
          headSha: newHeadSha,
          files: Array.from(next.values()),
        };
        persistToStorage(repoId, prNumber, state);
        return next;
      });
      return resetCount;
    },
    [repoId, prNumber],
  );

  // --- Staleness detection: reset viewed files when head SHA changes ---

  const prevHeadShaRef = useRef<string>(headSha);

  useEffect(() => {
    const prevSha = prevHeadShaRef.current;
    prevHeadShaRef.current = headSha;

    // Skip on initial mount or when SHA hasn't actually changed.
    if (!prevSha || prevSha === headSha) return;

    const resetCount = resetStaleFiles(headSha, files);
    if (resetCount > 0) {
      publishToast({
        kind: "info",
        title: `${resetCount} file${resetCount === 1 ? "" : "s"} reset due to new commits`,
      });
    }
  }, [headSha, files, resetStaleFiles]);

  // --- Context value ---

  const value: FileProgressContextValue = {
    markViewed,
    markUnviewed,
    isViewed,
    viewedCount: entries.size,
    totalFiles: files.length,
    resetStaleFiles,
  };

  return (
    <FileProgressContext.Provider value={value}>{children}</FileProgressContext.Provider>
  );
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

/**
 * Returns file progress tracking actions for the current PR.
 *
 * Must be called within a `<FileProgressProvider>`.
 */
export function useFileProgress(): FileProgressContextValue {
  const ctx = useContext(FileProgressContext);
  if (!ctx) {
    throw new Error("useFileProgress must be used within a FileProgressProvider");
  }
  return ctx;
}
