/**
 * Reactive store tracking LSP diagnostics per file URI.
 *
 * Subscribes to Monaco marker changes and provides hooks for:
 * - Tab badges (error/warning counts per file)
 * - File tree indicators
 * - Problems panel (all diagnostics across open files)
 */

import { useEffect, useMemo, useState } from "react";

// ── Types ───────────────────────────────────────────────────────────

export interface DiagnosticEntry {
  uri: string;
  startLineNumber: number;
  startColumn: number;
  endLineNumber: number;
  endColumn: number;
  severity: number; // Monaco MarkerSeverity: Hint=1, Info=2, Warning=4, Error=8
  message: string;
  source?: string;
  code?: string;
}

export interface DiagnosticCounts {
  errors: number;
  warnings: number;
  infos: number;
  hints: number;
}

// ── Store ───────────────────────────────────────────────────────────

type Listener = () => void;

class DiagnosticsStore {
  private diagnostics = new Map<string, DiagnosticEntry[]>();
  private listeners = new Set<Listener>();

  /** Update diagnostics for a given URI. */
  set(uri: string, entries: DiagnosticEntry[]): void {
    if (entries.length === 0) {
      this.diagnostics.delete(uri);
    } else {
      this.diagnostics.set(uri, entries);
    }
    this.notify();
  }

  /** Clear diagnostics for a URI. */
  clear(uri: string): void {
    if (this.diagnostics.has(uri)) {
      this.diagnostics.delete(uri);
      this.notify();
    }
  }

  /** Clear all diagnostics. */
  clearAll(): void {
    if (this.diagnostics.size > 0) {
      this.diagnostics.clear();
      this.notify();
    }
  }

  /** Get diagnostics for a specific URI. */
  getForUri(uri: string): DiagnosticEntry[] {
    return this.diagnostics.get(uri) ?? [];
  }

  /** Get counts for a specific URI. */
  getCountsForUri(uri: string): DiagnosticCounts {
    const entries = this.diagnostics.get(uri) ?? [];
    return countSeverities(entries);
  }

  /** Get all diagnostics grouped by URI. */
  getAll(): Map<string, DiagnosticEntry[]> {
    return new Map(this.diagnostics);
  }

  /** Get total counts across all files. */
  getTotalCounts(): DiagnosticCounts {
    const all: DiagnosticEntry[] = [];
    for (const entries of this.diagnostics.values()) {
      all.push(...entries);
    }
    return countSeverities(all);
  }

  /** Get all URIs that have diagnostics. */
  getUrisWithDiagnostics(): string[] {
    return Array.from(this.diagnostics.keys());
  }

  /** Subscribe to changes. Returns unsubscribe function. */
  subscribe(listener: Listener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  private notify(): void {
    for (const listener of this.listeners) {
      try {
        listener();
      } catch {
        // Don't let a listener error break the store
      }
    }
  }
}

function countSeverities(entries: DiagnosticEntry[]): DiagnosticCounts {
  let errors = 0;
  let warnings = 0;
  let infos = 0;
  let hints = 0;

  for (const entry of entries) {
    switch (entry.severity) {
      case 8:
        errors++;
        break;
      case 4:
        warnings++;
        break;
      case 2:
        infos++;
        break;
      case 1:
        hints++;
        break;
    }
  }

  return { errors, warnings, infos, hints };
}

/** Global singleton diagnostics store. */
export const diagnosticsStore = new DiagnosticsStore();

// ── Monaco integration ──────────────────────────────────────────────

/**
 * Initialize the diagnostics store by subscribing to Monaco marker changes.
 * Call this once when the editor is mounted.
 *
 * Uses `monaco.editor.onDidChangeMarkers` to track LSP diagnostics.
 */
export function initDiagnosticsStoreFromMonaco(monaco: typeof import("monaco-editor")): () => void {
  const disposable = monaco.editor.onDidChangeMarkers((uris) => {
    for (const uri of uris) {
      const uriStr = uri.toString();
      const markers = monaco.editor.getModelMarkers({ resource: uri });

      // Only track LSP markers (owner === "lsp")
      const lspMarkers = markers.filter((m) => m.owner === "lsp");

      const entries: DiagnosticEntry[] = lspMarkers.map((m) => ({
        uri: uriStr,
        startLineNumber: m.startLineNumber,
        startColumn: m.startColumn,
        endLineNumber: m.endLineNumber,
        endColumn: m.endColumn,
        severity: m.severity,
        message: m.message,
        source: m.source ?? undefined,
        code: typeof m.code === "object" ? m.code.value : m.code?.toString(),
      }));

      diagnosticsStore.set(uriStr, entries);
    }
  });

  return () => disposable.dispose();
}

// ── React Hooks ─────────────────────────────────────────────────────

/**
 * Hook that returns diagnostic counts for a specific file URI.
 * Re-renders when diagnostics change.
 */
export function useDiagnosticCounts(uri: string | null): DiagnosticCounts {
  const [counts, setCounts] = useState<DiagnosticCounts>({
    errors: 0,
    warnings: 0,
    infos: 0,
    hints: 0,
  });

  useEffect(() => {
    if (!uri) {
      setCounts({ errors: 0, warnings: 0, infos: 0, hints: 0 });
      return;
    }

    const update = () => setCounts(diagnosticsStore.getCountsForUri(uri));
    update();
    return diagnosticsStore.subscribe(update);
  }, [uri]);

  return counts;
}

/**
 * Hook that returns all diagnostics grouped by file.
 * Re-renders when any diagnostics change.
 */
export function useAllDiagnostics(): Map<string, DiagnosticEntry[]> {
  const [version, setVersion] = useState(0);

  useEffect(() => {
    return diagnosticsStore.subscribe(() => setVersion((v) => v + 1));
  }, []);

  return useMemo(() => diagnosticsStore.getAll(), [version]);
}

/**
 * Hook that returns total diagnostic counts across all files.
 */
export function useTotalDiagnosticCounts(): DiagnosticCounts {
  const [counts, setCounts] = useState<DiagnosticCounts>({
    errors: 0,
    warnings: 0,
    infos: 0,
    hints: 0,
  });

  useEffect(() => {
    const update = () => setCounts(diagnosticsStore.getTotalCounts());
    update();
    return diagnosticsStore.subscribe(update);
  }, []);

  return counts;
}

/**
 * Hook that returns diagnostic counts for a file path (converts to URI).
 * Useful for file tree indicators.
 */
export function useDiagnosticCountsForPath(
  repoRoot: string | null,
  relativePath: string | null,
): DiagnosticCounts {
  const uri = useMemo(() => {
    if (!repoRoot || !relativePath) return null;
    return `file://${repoRoot}/${relativePath}`;
  }, [repoRoot, relativePath]);

  return useDiagnosticCounts(uri);
}
