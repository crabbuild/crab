/**
 * Context Pin Panel — Persistent context that survives across agent iterations.
 *
 * Users pin snippets, notes, or clipboard content. Pinned items are stored
 * in the worktree's `.context/` folder and injected into every agent prompt.
 * Unlike chat history (which gets truncated), pinned context is always in
 * view for the agent.
 *
 * Features:
 * - Resizable left edge (drag to widen/narrow).
 * - File search autocomplete in the composer — type a path fragment and
 *   pick from matching repo files to pin their content directly.
 * - Manual text/paste mode for arbitrary snippets.
 * - Real delete via `orchestrator:remove-context-file` IPC.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import { ClipboardPaste, FileText, Pin, Search, Trash2, X } from "lucide-react";
import { OrchestratorTask } from "./TaskCard";
import { ResizeHandle } from "./ResizeHandle";
import { DirEntry, rpc } from "../../ipc";
import { publishToast } from "../../lib/toasts";

// ── Types ───────────────────────────────────────────────────────────

interface ContextFile {
  name: string;
  content: string;
  /** Size in bytes. */
  size: number;
}

interface Props {
  task: OrchestratorTask;
  /** Repo id for file search autocomplete. */
  repoId?: string | null;
  onClose: () => void;
}

// ── Helpers ─────────────────────────────────────────────────────────

/** Build a safe-ish filename stem from free-form content. */
function deriveFileStem(content: string): string {
  const firstLine = content.split("\n").find((l) => l.trim().length > 0) ?? "";
  const slug = firstLine
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 40);
  if (slug) return slug;
  return `snippet-${Date.now().toString(36)}`;
}

/** Render a human-readable size for the sidebar. */
function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/** Fuzzy match + rank file paths against a query. */
function fuzzyRankPath(entries: DirEntry[], q: string): DirEntry[] {
  const scored: Array<{ e: DirEntry; score: number }> = [];
  for (const e of entries) {
    const path = e.path.toLowerCase();
    const base = path.slice(path.lastIndexOf("/") + 1);
    const score = Math.max(matchScore(base, q) + 500, matchScore(path, q));
    if (score > 0) scored.push({ e, score });
  }
  scored.sort((a, b) => b.score - a.score);
  return scored.map((s) => s.e);
}

function matchScore(hay: string, q: string): number {
  let hi = 0, qi = 0, score = 0;
  let prevHit = -2;
  while (hi < hay.length && qi < q.length) {
    if (hay[hi] === q[qi]) {
      score += 10;
      if (prevHit === hi - 1) score += 15;
      prevHit = hi;
      qi++;
    }
    hi++;
  }
  return qi === q.length ? score - hi : 0;
}

// ── Persisted width ─────────────────────────────────────────────────

const STORAGE_KEY = "gx:pin-panel-width";
const DEFAULT_WIDTH = 280;
const MIN_WIDTH = 200;
const MAX_WIDTH = 500;

function loadWidth(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (stored) {
      const n = parseInt(stored, 10);
      if (n >= MIN_WIDTH && n <= MAX_WIDTH) return n;
    }
  } catch { /* ignore */ }
  return DEFAULT_WIDTH;
}

function saveWidth(w: number): void {
  try { localStorage.setItem(STORAGE_KEY, String(w)); } catch { /* ignore */ }
}

// ── Component ───────────────────────────────────────────────────────

export function ContextPinPanel({ task, repoId, onClose }: Props) {
  const [width, setWidth] = useState(loadWidth);
  const [files, setFiles] = useState<ContextFile[]>([]);
  const [loading, setLoading] = useState(true);

  // Composer state.
  const [composerName, setComposerName] = useState("");
  const [composerContent, setComposerContent] = useState("");
  const [pinning, setPinning] = useState(false);
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);

  // File search autocomplete state.
  const [fileEntries, setFileEntries] = useState<DirEntry[] | null>(null);
  const [showSuggestions, setShowSuggestions] = useState(false);
  const [selectedSuggestion, setSelectedSuggestion] = useState(0);
  const suggestionsRef = useRef<HTMLDivElement | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);

  // ── Resize ──────────────────────────────────────────────────────

  const handleResize = useCallback((delta: number) => {
    setWidth((w) => {
      // Negative delta = dragging left = making panel wider.
      const next = Math.max(MIN_WIDTH, Math.min(MAX_WIDTH, w - delta));
      saveWidth(next);
      return next;
    });
  }, []);

  // ── Load pinned files ───────────────────────────────────────────

  const loadFiles = useCallback(async () => {
    try {
      const result = await (window as any).crab?.orchestrator?.listContextFiles(task.id);
      if (result?.success && Array.isArray(result.files)) {
        setFiles(result.files as ContextFile[]);
      } else {
        setFiles([]);
      }
    } catch {
      setFiles([]);
    } finally {
      setLoading(false);
    }
  }, [task.id]);

  useEffect(() => { loadFiles(); }, [loadFiles]);

  // ── Load repo file list for autocomplete ────────────────────────

  useEffect(() => {
    if (!repoId) return;
    let cancelled = false;
    rpc.list(repoId, {})
      .then((result) => {
        if (!cancelled) {
          setFileEntries(result.entries.filter((e) => e.kind !== "dir" && e.kind !== "submodule"));
        }
      })
      .catch(() => { /* best-effort */ });
    return () => { cancelled = true; };
  }, [repoId]);

  // ── Autocomplete suggestions ────────────────────────────────────

  const suggestions = useMemo(() => {
    if (!fileEntries || !composerName.trim()) return [];
    return fuzzyRankPath(fileEntries, composerName.trim().toLowerCase()).slice(0, 12);
  }, [fileEntries, composerName]);

  // Close suggestions on outside click.
  useEffect(() => {
    if (!showSuggestions) return;
    const handler = (e: MouseEvent) => {
      if (
        suggestionsRef.current &&
        !suggestionsRef.current.contains(e.target as Node) &&
        inputRef.current &&
        !inputRef.current.contains(e.target as Node)
      ) {
        setShowSuggestions(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [showSuggestions]);

  // ── Pick a file from autocomplete ───────────────────────────────

  const handlePickFile = useCallback(async (entry: DirEntry) => {
    setShowSuggestions(false);
    setComposerName(entry.path);
    // Read the file content and populate the composer.
    if (!repoId) return;
    try {
      const result = await rpc.readText(repoId, entry.path, { max_bytes: 64 * 1024 });
      if (result.text != null) {
        setComposerContent(result.text);
      }
    } catch {
      // If read fails, just set the name — user can paste content manually.
    }
    requestAnimationFrame(() => textareaRef.current?.focus());
  }, [repoId]);

  // ── Composer logic ──────────────────────────────────────────────

  const effectiveName = useMemo(() => {
    const trimmed = composerName.trim();
    if (!trimmed && !composerContent.trim()) return "";
    const stem = trimmed || deriveFileStem(composerContent);
    // Preserve original extension if user typed one.
    if (stem.includes(".")) return stem;
    return `${stem}.md`;
  }, [composerName, composerContent]);

  const canPin = composerContent.trim().length > 0 && !pinning;

  const handlePin = useCallback(async () => {
    if (!canPin) return;
    setPinning(true);
    try {
      const res = await (window as any).crab?.orchestrator?.addContextFile(
        task.id,
        effectiveName,
        composerContent,
      );
      if (res?.success === false) {
        publishToast({ kind: "error", title: "Pin failed", body: res.error });
        return;
      }
      publishToast({ kind: "success", title: "Pinned", body: effectiveName });
      setComposerName("");
      setComposerContent("");
      await loadFiles();
    } catch (err: any) {
      publishToast({ kind: "error", title: "Pin failed", body: err?.message });
    } finally {
      setPinning(false);
    }
  }, [canPin, task.id, effectiveName, composerContent, loadFiles]);

  const handleRemove = useCallback(
    async (fileName: string) => {
      try {
        const res = await (window as any).crab?.orchestrator?.removeContextFile?.(
          task.id,
          fileName,
        );
        if (res?.success === false) {
          publishToast({ kind: "error", title: "Unpin failed", body: res.error });
          return;
        }
        await loadFiles();
      } catch (err: any) {
        publishToast({ kind: "error", title: "Unpin failed", body: err?.message });
      }
    },
    [task.id, loadFiles],
  );

  const handlePasteFromClipboard = useCallback(async () => {
    try {
      const text = await navigator.clipboard.readText();
      if (!text.trim()) {
        publishToast({ kind: "info", title: "Clipboard is empty" });
        return;
      }
      setComposerContent((prev) => (prev ? `${prev}\n${text}` : text));
      requestAnimationFrame(() => textareaRef.current?.focus());
    } catch {
      publishToast({ kind: "error", title: "Clipboard access denied" });
    }
  }, []);

  // Cmd/Ctrl-Enter to pin from the composer.
  const handleTextareaKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
        e.preventDefault();
        handlePin();
      }
    },
    [handlePin],
  );

  // Keyboard nav for autocomplete.
  const handleInputKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLInputElement>) => {
      if (!showSuggestions || suggestions.length === 0) {
        if (e.key === "ArrowDown" && suggestions.length > 0) {
          setShowSuggestions(true);
          setSelectedSuggestion(0);
          e.preventDefault();
        }
        return;
      }
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setSelectedSuggestion((s) => Math.min(s + 1, suggestions.length - 1));
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setSelectedSuggestion((s) => Math.max(s - 1, 0));
      } else if (e.key === "Enter") {
        e.preventDefault();
        if (suggestions[selectedSuggestion]) {
          handlePickFile(suggestions[selectedSuggestion]);
        }
      } else if (e.key === "Escape") {
        e.preventDefault();
        setShowSuggestions(false);
      }
    },
    [showSuggestions, suggestions, selectedSuggestion, handlePickFile],
  );

  return (
    <>
      {/* Resize handle on the left edge of the panel */}
      <ResizeHandle direction="horizontal" onResize={handleResize} />

      <div
        className="flex h-full shrink-0 flex-col overflow-hidden border-l border-gx-border bg-gx-surface"
        style={{ width }}
      >
        {/* Header */}
        <div className="flex items-center gap-2 border-b border-gx-border px-3 py-2">
          <Pin size={12} className="text-gx-accent" />
          <span className="flex-1 text-xs font-medium text-gx-text">Pinned Context</span>
          <span className="text-[10px] text-gx-muted">{files.length}</span>
          <button
            type="button"
            onClick={onClose}
            className="rounded p-0.5 text-gx-muted hover:text-gx-text"
            aria-label="Close context panel"
          >
            <X size={12} />
          </button>
        </div>

        {/* Description */}
        <div className="border-b border-gx-border/50 px-3 py-2 text-[10px] text-gx-muted">
          Pinned items are included in every agent prompt. Use them for specs,
          conventions, or reference snippets the agent shouldn't forget.
        </div>

        {/* File list */}
        <div className="min-h-0 flex-1 overflow-auto">
          {loading ? (
            <div className="flex items-center justify-center py-8 text-[11px] text-gx-muted">
              Loading…
            </div>
          ) : files.length === 0 ? (
            <div className="flex flex-col items-center justify-center gap-2 px-4 py-8 text-center">
              <Pin size={18} className="text-gx-muted/50" />
              <span className="text-[11px] text-gx-muted">No pinned context yet</span>
              <span className="text-[10px] text-gx-muted/70">
                Search for a file below or paste content to pin.
              </span>
            </div>
          ) : (
            <ul className="divide-y divide-gx-border/30">
              {files.map((file) => (
                <li key={file.name} className="group px-3 py-2">
                  <div className="flex items-center gap-2">
                    <FileText size={11} className="shrink-0 text-gx-accent" />
                    <span className="flex-1 truncate text-[11px] font-medium text-gx-text">
                      {file.name}
                    </span>
                    <span className="shrink-0 text-[10px] text-gx-muted/70">{formatSize(file.size)}</span>
                    <button
                      type="button"
                      onClick={() => handleRemove(file.name)}
                      className="rounded p-0.5 text-gx-muted opacity-0 transition hover:text-gx-error group-hover:opacity-100 focus:opacity-100"
                      aria-label={`Unpin ${file.name}`}
                      title="Unpin"
                    >
                      <Trash2 size={10} />
                    </button>
                  </div>
                  {file.content.trim().length > 0 && (
                    <div className="mt-1 line-clamp-2 break-all font-mono text-[10px] text-gx-muted">
                      {file.content.slice(0, 160)}
                      {file.content.length > 160 && "…"}
                    </div>
                  )}
                </li>
              ))}
            </ul>
          )}
        </div>

        {/* Composer — the single entry point for adding context. */}
        <div className="shrink-0 border-t border-gx-border bg-gx-bg-sub/40 px-3 py-2">
          {/* File search input with autocomplete */}
          <div className="relative">
            <div className="flex items-center gap-1.5 rounded border border-gx-border bg-gx-bg-sub px-2 py-1">
              <Search size={10} className="shrink-0 text-gx-muted" />
              <input
                ref={inputRef}
                type="text"
                value={composerName}
                onChange={(e) => {
                  setComposerName(e.target.value);
                  setShowSuggestions(e.target.value.trim().length > 0 && (fileEntries?.length ?? 0) > 0);
                  setSelectedSuggestion(0);
                }}
                onFocus={() => {
                  if (composerName.trim() && suggestions.length > 0) {
                    setShowSuggestions(true);
                  }
                }}
                onKeyDown={handleInputKeyDown}
                placeholder={composerContent.trim() ? effectiveName : "Search files or type a name…"}
                autoComplete="off"
                spellCheck={false}
                className="min-w-0 flex-1 bg-transparent text-[10px] text-gx-text placeholder:text-gx-muted/60 outline-none"
                aria-autocomplete="list"
                aria-expanded={showSuggestions && suggestions.length > 0}
                role="combobox"
              />
            </div>

            {/* Autocomplete dropdown */}
            {showSuggestions && suggestions.length > 0 && (
              <div
                ref={suggestionsRef}
                className="absolute bottom-full left-0 right-0 z-10 mb-1 max-h-48 overflow-y-auto rounded border border-gx-border bg-gx-bg shadow-lg"
                role="listbox"
              >
                {suggestions.map((entry, i) => (
                  <button
                    key={entry.path}
                    type="button"
                    onClick={() => handlePickFile(entry)}
                    onMouseEnter={() => setSelectedSuggestion(i)}
                    className={clsx(
                      "flex w-full items-center gap-2 px-2.5 py-1.5 text-left text-[10px] transition",
                      i === selectedSuggestion
                        ? "bg-gx-accent/10 text-gx-text"
                        : "text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
                    )}
                    role="option"
                    aria-selected={i === selectedSuggestion}
                  >
                    <FileText size={10} className="shrink-0 text-gx-accent/70" />
                    <span className="flex-1 truncate">
                      <span className="font-medium text-gx-text">
                        {entry.path.split("/").pop()}
                      </span>
                      {entry.path.includes("/") && (
                        <span className="ml-1 text-gx-muted/70">
                          {entry.path.slice(0, entry.path.lastIndexOf("/"))}
                        </span>
                      )}
                    </span>
                  </button>
                ))}
              </div>
            )}
          </div>

          {/* Content textarea */}
          <textarea
            ref={textareaRef}
            value={composerContent}
            onChange={(e) => setComposerContent(e.target.value)}
            onKeyDown={handleTextareaKeyDown}
            placeholder="Paste or type context to pin…"
            rows={3}
            className="mt-2 w-full resize-none rounded border border-gx-border bg-gx-bg-sub px-2 py-1.5 font-mono text-[10px] text-gx-text placeholder:text-gx-muted focus:border-gx-accent focus:outline-none"
          />

          {/* Actions row */}
          <div className="mt-1.5 flex items-center gap-1.5">
            <div className="flex-1" />
            <button
              type="button"
              onClick={handlePasteFromClipboard}
              className="flex shrink-0 items-center gap-1 rounded px-1.5 py-1 text-[10px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
              title="Paste from clipboard"
              aria-label="Paste from clipboard"
            >
              <ClipboardPaste size={11} />
            </button>
            <button
              type="button"
              onClick={handlePin}
              disabled={!canPin}
              className="shrink-0 rounded bg-gx-accent px-2.5 py-1 text-[10px] font-medium text-white transition hover:bg-gx-accent-hover disabled:cursor-not-allowed disabled:opacity-40"
              title="Pin (⌘⏎)"
            >
              {pinning ? "Pinning…" : "Pin"}
            </button>
          </div>
        </div>
      </div>
    </>
  );
}
