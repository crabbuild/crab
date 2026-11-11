import { Button } from "@/components/ui/button";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Editor, Monaco, OnMount } from "@monaco-editor/react";
import type { editor } from "monaco-editor";
import { Loader2, Save, X, ChevronLeft, ChevronRight } from "lucide-react";
import { rpc } from "../ipc";
import { languageForPath } from "../lib/language";
import { useSettings } from "../settings";
import { useEffectiveTheme } from "../hooks/useEffectiveTheme";
import {
  ConflictRegion,
  parseConflictMarkers,
  resolveConflict,
} from "../lib/parseConflictMarkers";
import { PointerConflictCard } from "./PointerConflictCard";
import type { PointerPatchKind } from "../lib/pointer-detection";

// Signatures for detecting pointer content in raw file bodies.
// These match the same patterns as `isPointerPatch` in
// `lib/pointer-detection.ts`, but applied to raw file content
// rather than unified patches.
const POINTER_SIGNATURES: Array<{ pattern: string; kind: PointerPatchKind }> = [
  { pattern: "crab.dev/v1", kind: "crab" },
  { pattern: "version crab v1", kind: "crab" },
  { pattern: "git-lfs.github.com/spec/v1", kind: "lfs" },
  { pattern: "version https://git-lfs.github.com/", kind: "lfs" },
];

/**
 * Check raw file content for a Crab or LFS pointer signature.
 * Returns the detected kind, or `null` if no pointer is found.
 */
function detectPointerContent(content: string | null): PointerPatchKind | null {
  if (!content || content.length === 0) return null;
  const haystack = content.toLowerCase();
  for (const { pattern, kind } of POINTER_SIGNATURES) {
    if (haystack.includes(pattern)) return kind;
  }
  return null;
}

interface Props {
  repoId: string;
  path: string;
  onResolved: (path: string) => void;
  onCancel: () => void;
  /** Navigate to the next conflicted file. */
  onNextFile?: () => void;
  /** Navigate to the previous conflicted file. */
  onPrevFile?: () => void;
  /** Label showing position, e.g. "2 of 5". */
  filePositionLabel?: string;
}

interface Sides {
  base: string | null;
  current: string | null;
  incoming: string | null;
}

/**
 * 3-way merge editor using Monaco.
 *
 * Layout:
 *   Top row: Base (read-only) | Current/Ours (read-only) | Incoming/Theirs (read-only)
 *   Bottom row: Result pane (editable) + Save Resolution button
 *
 * On mount, fetches the three sides via `rpc.conflictReadSides` and
 * pre-fills the result pane with the working-tree content (which has
 * conflict markers) via `rpc.readText`.
 */
export function MergeEditor({ repoId, path, onResolved, onCancel, onNextFile, onPrevFile, filePositionLabel }: Props) {
  const [sides, setSides] = useState<Sides | null>(null);
  const [resultContent, setResultContent] = useState<string>("");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const { settings } = useSettings();
  const effective = useEffectiveTheme();
  const monacoRef = useRef<Monaco | null>(null);
  const resultEditorRef = useRef<editor.IStandaloneCodeEditor | null>(null);

  // Parse conflict markers from the result content
  const conflicts = useMemo(
    () => parseConflictMarkers(resultContent),
    [resultContent]
  );

  // Detect whether any conflict side contains a pointer signature.
  // When detected, we show a PointerConflictCard instead of the
  // 3-pane Monaco editor — pointer blobs can't be meaningfully
  // merged inline.
  const pointerKind = useMemo<PointerPatchKind | null>(() => {
    if (!sides) return null;
    return (
      detectPointerContent(sides.base) ??
      detectPointerContent(sides.current) ??
      detectPointerContent(sides.incoming)
    );
  }, [sides]);

  // Resolve a conflict by index
  const handleResolve = useCallback(
    (index: number, resolution: "current" | "incoming" | "both") => {
      const region = conflicts[index];
      if (!region) return;
      setResultContent((prev) => resolveConflict(prev, region, resolution));
    },
    [conflicts]
  );

  // Find the conflict nearest to the cursor position in the result editor
  const resolveNearestConflict = useCallback(
    (resolution: "current" | "incoming" | "both") => {
      if (conflicts.length === 0) return;

      const ed = resultEditorRef.current;
      const cursorLine = ed ? ed.getPosition()?.lineNumber ?? 1 : 1;
      // Monaco lines are 1-based, our regions are 0-based
      const cursorIdx = cursorLine - 1;

      let nearest = 0;
      let minDist = Infinity;
      for (let i = 0; i < conflicts.length; i++) {
        const region = conflicts[i];
        // Distance: 0 if cursor is inside the region, otherwise distance to nearest edge
        let dist: number;
        if (cursorIdx >= region.startLine && cursorIdx <= region.endLine) {
          dist = 0;
        } else {
          dist = Math.min(
            Math.abs(cursorIdx - region.startLine),
            Math.abs(cursorIdx - region.endLine)
          );
        }
        if (dist < minDist) {
          minDist = dist;
          nearest = i;
        }
      }

      handleResolve(nearest, resolution);
    },
    [conflicts, handleResolve]
  );

  // Keyboard shortcuts: Ctrl+Shift+1/2/3 for resolve, Alt+↓/↑ for next/prev conflict
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      // Alt+Down / Alt+Up: navigate between conflicts
      if (e.altKey && !e.ctrlKey && !e.metaKey && !e.shiftKey) {
        if (e.key === "ArrowDown") {
          e.preventDefault();
          navigateToConflict("next");
          return;
        }
        if (e.key === "ArrowUp") {
          e.preventDefault();
          navigateToConflict("prev");
          return;
        }
      }
      if (!e.ctrlKey || !e.shiftKey) return;
      if (e.key === "!" || e.code === "Digit1") {
        e.preventDefault();
        resolveNearestConflict("current");
      } else if (e.key === "@" || e.code === "Digit2") {
        e.preventDefault();
        resolveNearestConflict("incoming");
      } else if (e.key === "#" || e.code === "Digit3") {
        e.preventDefault();
        resolveNearestConflict("both");
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [resolveNearestConflict]);

  // Navigate to next/prev conflict in the result editor
  const navigateToConflict = useCallback(
    (direction: "next" | "prev") => {
      if (conflicts.length === 0) return;
      const ed = resultEditorRef.current;
      if (!ed) return;

      const cursorLine = (ed.getPosition()?.lineNumber ?? 1) - 1; // 0-based
      let targetIdx = -1;

      if (direction === "next") {
        // Find the first conflict that starts after the cursor
        for (let i = 0; i < conflicts.length; i++) {
          if (conflicts[i].startLine > cursorLine) {
            targetIdx = i;
            break;
          }
        }
        // Wrap around to first conflict
        if (targetIdx === -1) targetIdx = 0;
      } else {
        // Find the last conflict that starts before the cursor
        for (let i = conflicts.length - 1; i >= 0; i--) {
          if (conflicts[i].startLine < cursorLine) {
            targetIdx = i;
            break;
          }
        }
        // Wrap around to last conflict
        if (targetIdx === -1) targetIdx = conflicts.length - 1;
      }

      const target = conflicts[targetIdx];
      if (target) {
        const line = target.startLine + 1; // 1-based
        ed.revealLineInCenter(line);
        ed.setPosition({ lineNumber: line, column: 1 });
        ed.focus();
      }
    },
    [conflicts],
  );

  // Fetch conflict sides + working-tree content on mount
  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);

    Promise.all([
      rpc.conflictReadSides(repoId, path),
      rpc.readText(repoId, path),
    ])
      .then(([sidesResult, textResult]) => {
        if (cancelled) return;
        setSides(sidesResult);
        setResultContent(textResult.text ?? "");
      })
      .catch((e: Error) => {
        if (!cancelled) setError(e.message);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });

    return () => { cancelled = true; };
  }, [repoId, path]);

  const handleSave = useCallback(async () => {
    setSaving(true);
    try {
      await rpc.gitApplyResolution(repoId, path, resultContent);
      onResolved(path);
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }, [repoId, path, resultContent, onResolved]);

  // Auto-scroll to the first conflict marker when the result editor mounts
  // or when the content first loads with conflicts.
  const hasScrolledRef = useRef(false);
  useEffect(() => {
    if (hasScrolledRef.current) return;
    if (conflicts.length === 0) return;
    const ed = resultEditorRef.current;
    if (!ed) return;
    // Scroll to the first conflict marker (1-based line number)
    const firstLine = conflicts[0].startLine + 1;
    ed.revealLineInCenter(firstLine);
    ed.setPosition({ lineNumber: firstLine, column: 1 });
    hasScrolledRef.current = true;
  }, [conflicts]);

  // Reset scroll tracking when path changes
  useEffect(() => {
    hasScrolledRef.current = false;
  }, [path]);

  const themeName = effective === "light" ? "github-light" : "github-dark";

  const handleMonacoMount: OnMount = (_editor, monaco) => {
    monacoRef.current = monaco;
    monaco.editor.setTheme(themeName);
  };

  // Keep theme in sync when it changes
  useEffect(() => {
    if (monacoRef.current) {
      monacoRef.current.editor.setTheme(themeName);
    }
  }, [themeName]);

  const editorOptions: editor.IStandaloneEditorConstructionOptions = {
    minimap: { enabled: false },
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, Monaco, monospace",
    fontSize: settings.editorFontSize,
    lineNumbers: "on",
    scrollBeyondLastLine: false,
    renderLineHighlight: "all",
    wordWrap: settings.editorWordWrap ? "on" : "off",
    smoothScrolling: true,
    automaticLayout: true,
    padding: { top: 4, bottom: 4 },
  };

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground">
        <Loader2 size={14} className="animate-spin" /> Loading conflict sides…
      </div>
    );
  }

  if (error && !sides) {
    return (
      <div className="m-4 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
        Failed to load conflict data for <code className="font-mono">{path}</code>: {error}
      </div>
    );
  }

  const language = languageForPath(path);

  return (
    <div className="flex h-full flex-col bg-background">
      {/* Header */}
      <div className="flex shrink-0 items-center justify-between border-b border-border bg-card px-3 py-1.5">
        <div className="flex items-center gap-2 text-xs text-foreground">
          <span className="font-medium">Merge Editor</span>
          <span className="text-muted-foreground">—</span>
          <span className="font-mono text-muted-foreground">{path}</span>
          {filePositionLabel && (
            <span className="rounded bg-background px-1.5 py-0.5 text-[10px] text-muted-foreground">
              {filePositionLabel}
            </span>
          )}
        </div>
        <div className="flex items-center gap-2">
          {/* File navigation */}
          {(onPrevFile || onNextFile) && (
            <div className="flex items-center gap-0.5">
              <button
                type="button"
                className="flex h-6 w-6 items-center justify-center rounded text-muted-foreground hover:bg-accent hover:text-foreground disabled:opacity-30 disabled:hover:bg-transparent disabled:hover:text-muted-foreground"
                onClick={onPrevFile}
                disabled={!onPrevFile}
                aria-label="Previous conflict file"
                title="Previous file"
              >
                <ChevronLeft size={14} />
              </button>
              <button
                type="button"
                className="flex h-6 w-6 items-center justify-center rounded text-muted-foreground hover:bg-accent hover:text-foreground disabled:opacity-30 disabled:hover:bg-transparent disabled:hover:text-muted-foreground"
                onClick={onNextFile}
                disabled={!onNextFile}
                aria-label="Next conflict file"
                title="Next file"
              >
                <ChevronRight size={14} />
              </button>
            </div>
          )}
          <Button variant="default"
            type="button"
            className="flex items-center gap-1.5 text-xs"
            onClick={handleSave}
            disabled={saving}
            aria-label="Save resolution"
          >
            {saving ? <Loader2 size={12} className="animate-spin" /> : <Save size={12} />}
            Save Resolution
          </Button>
          <button
            type="button"
            className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
            onClick={onCancel}
            aria-label="Close merge editor"
            title="Close"
          >
            <X size={14} />
          </button>
        </div>
      </div>

      {error && (
        <div className="shrink-0 border-b border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
          {error}
        </div>
      )}

      {/* Top row: 3 read-only panes (hidden for pointer conflicts) */}
      {pointerKind ? (
        <div className="flex min-h-0 flex-1 items-start justify-center overflow-y-auto border-b border-border">
          <PointerConflictCard path={path} kind={pointerKind} />
        </div>
      ) : (
        <div className="flex min-h-0 flex-1 border-b border-border">
          <SidePane
            title="Base"
            content={sides?.base ?? ""}
            language={language}
            theme={themeName}
            options={editorOptions}
            onMount={handleMonacoMount}
          />
          <div className="w-px shrink-0 bg-border" />
          <SidePane
            title="Current (Ours)"
            content={sides?.current ?? ""}
            language={language}
            theme={themeName}
            options={editorOptions}
            baseContent={sides?.base ?? undefined}
          />
          <div className="w-px shrink-0 bg-border" />
          <SidePane
            title="Incoming (Theirs)"
            content={sides?.incoming ?? ""}
            language={language}
            theme={themeName}
            options={editorOptions}
            baseContent={sides?.base ?? undefined}
          />
        </div>
      )}

      {/* Bottom row: editable result pane */}
      <div className="flex min-h-0 flex-1 flex-col">
        <div className="shrink-0 border-b border-border bg-card px-3 py-1 text-[10px] uppercase tracking-wider text-muted-foreground">
          Result (editable)
        </div>
        {conflicts.length > 0 && (
          <ConflictToolbar
            conflicts={conflicts}
            onResolve={handleResolve}
            onNavigate={navigateToConflict}
          />
        )}
        <div className="min-h-0 flex-1">
          <Editor
            path={`merge-result::${path}`}
            defaultLanguage={language}
            value={resultContent}
            onChange={(v) => setResultContent(v ?? "")}
            theme={themeName}
            onMount={(ed) => { resultEditorRef.current = ed; }}
            options={{
              ...editorOptions,
              readOnly: false,
              renderLineHighlight: "all",
            }}
          />
        </div>
      </div>
    </div>
  );
}

/** A single read-only pane in the top row with diff decorations. */
function SidePane({
  title,
  content,
  language,
  theme,
  options,
  onMount,
  baseContent,
}: {
  title: string;
  content: string;
  language: string;
  theme: string;
  options: editor.IStandaloneEditorConstructionOptions;
  onMount?: OnMount;
  /** Base content to diff against for highlighting changed lines. */
  baseContent?: string;
}) {
  const editorRef = useRef<editor.IStandaloneCodeEditor | null>(null);
  const decorationsRef = useRef<string[]>([]);

  // Apply diff decorations when content or base changes
  useEffect(() => {
    const ed = editorRef.current;
    if (!ed || !baseContent) return;

    const baseLines = baseContent.split("\n");
    const currentLines = content.split("\n");
    const decorations: editor.IModelDeltaDecoration[] = [];

    // Simple line-by-line diff: highlight lines that differ from base
    const maxLines = Math.max(baseLines.length, currentLines.length);
    for (let i = 0; i < maxLines; i++) {
      if (i >= baseLines.length) {
        // Line added (not in base)
        decorations.push({
          range: { startLineNumber: i + 1, startColumn: 1, endLineNumber: i + 1, endColumn: 1 },
          options: {
            isWholeLine: true,
            className: "diff-line-added",
            glyphMarginClassName: "diff-glyph-added",
          },
        });
      } else if (i >= currentLines.length) {
        // Line removed — can't highlight in this pane (it's shorter)
        break;
      } else if (baseLines[i] !== currentLines[i]) {
        // Line modified
        decorations.push({
          range: { startLineNumber: i + 1, startColumn: 1, endLineNumber: i + 1, endColumn: 1 },
          options: {
            isWholeLine: true,
            className: "diff-line-modified",
            glyphMarginClassName: "diff-glyph-modified",
          },
        });
      }
    }

    decorationsRef.current = ed.deltaDecorations(decorationsRef.current, decorations);
  }, [content, baseContent]);

  const handleMount: OnMount = (ed, monaco) => {
    editorRef.current = ed;
    onMount?.(ed, monaco);
  };

  return (
    <div className="flex min-w-0 flex-1 flex-col">
      <div className="shrink-0 border-b border-border bg-card px-3 py-1 text-[10px] uppercase tracking-wider text-muted-foreground">
        {title}
      </div>
      <div className="min-h-0 flex-1">
        <Editor
          path={`merge-${title.toLowerCase().replace(/[^a-z]/g, "")}::${content.slice(0, 20)}`}
          defaultLanguage={language}
          value={content}
          theme={theme}
          onMount={handleMount}
          options={{
            ...options,
            readOnly: true,
            glyphMargin: !!baseContent,
          }}
        />
      </div>
    </div>
  );
}

/** Toolbar listing each conflict with one-click resolution buttons. */
function ConflictToolbar({
  conflicts,
  onResolve,
  onNavigate,
}: {
  conflicts: ConflictRegion[];
  onResolve: (index: number, resolution: "current" | "incoming" | "both") => void;
  onNavigate: (direction: "next" | "prev") => void;
}) {
  return (
    <div className="shrink-0 overflow-x-auto border-b border-border bg-card px-3 py-1.5">
      <div className="flex items-center gap-3 text-xs">
        {/* Navigation buttons */}
        <div className="flex items-center gap-0.5">
          <button
            type="button"
            className="flex h-5 w-5 items-center justify-center rounded text-muted-foreground hover:bg-accent hover:text-foreground"
            onClick={() => onNavigate("prev")}
            title="Previous conflict (Alt+↑)"
            aria-label="Previous conflict"
          >
            <ChevronLeft size={12} />
          </button>
          <button
            type="button"
            className="flex h-5 w-5 items-center justify-center rounded text-muted-foreground hover:bg-accent hover:text-foreground"
            onClick={() => onNavigate("next")}
            title="Next conflict (Alt+↓)"
            aria-label="Next conflict"
          >
            <ChevronRight size={12} />
          </button>
        </div>

        <span className="font-medium text-foreground">
          {conflicts.length} conflict{conflicts.length !== 1 ? "s" : ""}
        </span>
        {conflicts.map((region, i) => (
          <div
            key={`${region.startLine}-${region.endLine}`}
            className="flex items-center gap-1 rounded border border-border bg-background px-2 py-0.5"
          >
            <span className="text-muted-foreground">
              L{region.startLine + 1}–{region.endLine + 1}
            </span>
            <button
              type="button"
              className="rounded px-1.5 py-0.5 text-[10px] font-medium text-info hover:bg-info/10"
              onClick={() => onResolve(i, "current")}
              title="Accept Current (Ctrl+Shift+1)"
              aria-label={`Accept current for conflict at line ${region.startLine + 1}`}
            >
              Current
            </button>
            <button
              type="button"
              className="rounded px-1.5 py-0.5 text-[10px] font-medium text-success hover:bg-success/10"
              onClick={() => onResolve(i, "incoming")}
              title="Accept Incoming (Ctrl+Shift+2)"
              aria-label={`Accept incoming for conflict at line ${region.startLine + 1}`}
            >
              Incoming
            </button>
            <button
              type="button"
              className="rounded px-1.5 py-0.5 text-[10px] font-medium text-warning hover:bg-warning/10"
              onClick={() => onResolve(i, "both")}
              title="Accept Both (Ctrl+Shift+3)"
              aria-label={`Accept both for conflict at line ${region.startLine + 1}`}
            >
              Both
            </button>
          </div>
        ))}
      </div>
    </div>
  );
}
