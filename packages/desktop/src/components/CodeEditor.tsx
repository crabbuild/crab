import { useEffect, useRef } from "react";
import * as monaco from "monaco-editor";
import { languageForPath } from "../lib/language";
import { useSettings } from "../settings";
import { useEffectiveTheme } from "../hooks/useEffectiveTheme";
import { isShikiWired, initShikiMonaco, loadTheme } from "../lib/shiki-monaco";
import { useLspClient } from "../lib/lsp-registry";
import { deriveEditorOptions } from "../lib/lsp-editor-options";

interface Props {
  path: string;
  /** Unique key for the model cache — should include repo context to
   *  prevent edits leaking between worktrees with the same relative path. */
  cacheKey?: string;
  value: string;
  readOnly?: boolean;
  /** Optional line number to reveal and highlight on mount. */
  line?: number;
  /** Called when the editor content changes (only fires when not readOnly). */
  onChange?: (value: string) => void;
  /** Called when the user presses Cmd/Ctrl+S. */
  onSave?: (value: string) => void;
  /** Called when the editor loses focus — used to flush pending saves. */
  onBlur?: (value: string) => void;
  /** Repo root path on disk — used to construct file:// URIs for LSP. */
  repoRoot?: string;
}

/**
 * Resolve the theme. If Shiki has been wired in, use the user's
 * selected theme from settings. Otherwise fall back to Monaco built-ins.
 */
function themeFor(effective: "dark" | "light", darkTheme: string, lightTheme: string): string {
  if (isShikiWired()) {
    return effective === "light" ? lightTheme : darkTheme;
  }
  return effective === "light" ? "vs" : "vs-dark";
}

/**
 * Force-tokenize visible lines immediately, bypassing Monaco's
 * requestIdleCallback scheduler which can delay highlighting by
 * seconds when the main thread is busy.
 *
 * Monaco's `model.tokenization.forceTokenization(line)` synchronously
 * tokenizes up to the given line. We call it for the visible range
 * so the user sees colors instantly, then let background tokenization
 * handle the rest of the file.
 */
function forceTokenizeVisible(editor: monaco.editor.IStandaloneCodeEditor) {
  const model = editor.getModel();
  if (!model) return;
  const visibleRanges = editor.getVisibleRanges();
  if (visibleRanges.length === 0) return;
  // Tokenize up to the last visible line (plus a small buffer).
  const lastVisibleLine = Math.min(
    visibleRanges[visibleRanges.length - 1].endLineNumber + 50,
    model.getLineCount(),
  );
  try {
    // The tokenization namespace exists at runtime but may not be in the
    // type definitions for all Monaco versions.
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (model as any).tokenization?.forceTokenization(lastVisibleLine);
  } catch {
    // forceTokenization can throw if no tokenizer is registered yet.
  }
}

/**
 * Monaco editor that renders IMMEDIATELY with instant syntax coloring.
 *
 * Key performance techniques:
 * 1. Editor created synchronously — no waiting for Shiki
 * 2. After creation, `forceTokenization` is called on visible lines
 *    to bypass Monaco's requestIdleCallback delay
 * 3. File switching reuses the editor instance (model swap, no remount)
 * 4. Models are cached so re-opening a file is instant
 * 5. Shiki loads in background; once ready, Monaco auto-retokenizes
 */
export function CodeEditor({ path, cacheKey, value, readOnly = true, line, onChange, onSave, onBlur, repoRoot }: Props) {
  const { settings } = useSettings();
  const effective = useEffectiveTheme();
  const containerRef = useRef<HTMLDivElement>(null);
  const editorRef = useRef<monaco.editor.IStandaloneCodeEditor | null>(null);
  const onChangeRef = useRef(onChange);
  const onSaveRef = useRef(onSave);
  const onBlurRef = useRef(onBlur);
  const suppressChangeRef = useRef(false);
  const { editorThemeDark, editorThemeLight } = settings;

  // The cache key scopes models per-worktree to prevent edits leaking
  // between worktrees that share the same relative file path.
  const modelKey = cacheKey ?? path;

  // The Monaco model URI must be an absolute file:// path matching what
  // we send to the LSP server — otherwise diagnostics can't be attached
  // to the correct model. When repoRoot is available we use the real
  // absolute path; otherwise we fall back to a synthetic file:// URI.
  const modelUri = repoRoot
    ? monaco.Uri.file(`${repoRoot}/${path}`)
    : monaco.Uri.parse(`file:///${path}`);
  const fileUri = modelUri.toString();

  // Keep refs in sync so the editor callbacks always see the latest.
  onChangeRef.current = onChange;
  onSaveRef.current = onSave;
  onBlurRef.current = onBlur;

  // Create the editor immediately on mount.
  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const lang = languageForPath(path);
    suppressChangeRef.current = true;
    const model = getOrCreateModel(modelKey, modelUri, lang, value);
    suppressChangeRef.current = false;
    const editor = monaco.editor.create(container, {
      model,
      readOnly,
      minimap: { enabled: settings.editorMinimap, renderCharacters: false },
      fontFamily: "ui-monospace, SFMono-Regular, Menlo, Monaco, monospace",
      fontSize: settings.editorFontSize,
      lineNumbers: "on",
      scrollBeyondLastLine: false,
      renderLineHighlight: "all",
      wordWrap: settings.editorWordWrap ? "on" : "off",
      smoothScrolling: true,
      cursorBlinking: "smooth",
      automaticLayout: true,
      bracketPairColorization: { enabled: true },
      guides: { indentation: true, highlightActiveIndentation: true },
      padding: { top: 8, bottom: 8 },
      theme: themeFor(effective, editorThemeDark, editorThemeLight),
    });
    editorRef.current = editor;

    // Wire up content change listener.
    editor.onDidChangeModelContent(() => {
      if (suppressChangeRef.current) return;
      if (onChangeRef.current) {
        onChangeRef.current(editor.getValue());
      }
    });

    // Cmd/Ctrl+S save keybinding.
    editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, () => {
      if (onSaveRef.current) {
        onSaveRef.current(editor.getValue());
      }
    });

    // Fallback: listen for Cmd/Ctrl+S on the container in case
    // Monaco's addCommand doesn't fire (e.g. focus edge cases).
    const handleKeyDown = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "s") {
        e.preventDefault();
        if (onSaveRef.current && editorRef.current) {
          onSaveRef.current(editorRef.current.getValue());
        }
      }
    };
    container.addEventListener("keydown", handleKeyDown);

    // Flush pending saves when the editor loses focus.
    editor.onDidBlurEditorWidget(() => {
      if (onBlurRef.current && editorRef.current) {
        onBlurRef.current(editorRef.current.getValue());
      }
    });

    // Force immediate tokenization of visible lines.
    requestAnimationFrame(() => forceTokenizeVisible(editor));

    return () => {
      container.removeEventListener("keydown", handleKeyDown);
      editor.dispose();
      editorRef.current = null;
    };
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // Swap model when path/value changes (the fast path — no remount).
  useEffect(() => {
    const editor = editorRef.current;
    if (!editor) return;
    const lang = languageForPath(path);
    suppressChangeRef.current = true;
    const model = getOrCreateModel(modelKey, modelUri, lang, value);
    const current = editor.getModel();
    if (current !== model) {
      editor.setModel(model);
      requestAnimationFrame(() => forceTokenizeVisible(editor));
    } else if (model.getValue() !== value) {
      model.setValue(value);
      requestAnimationFrame(() => forceTokenizeVisible(editor));
    }
    suppressChangeRef.current = false;
  }, [path, value, modelKey]);

  // Jump to line.
  useEffect(() => {
    const editor = editorRef.current;
    if (!editor || !line || line <= 0) return;
    editor.revealLineInCenter(line);
    editor.setPosition({ lineNumber: line, column: 1 });
    editor.setSelection({
      startLineNumber: line,
      startColumn: 1,
      endLineNumber: line,
      endColumn: 1000,
    });
  }, [line, path]);

  // Theme changes.
  useEffect(() => {
    if (!editorRef.current) return;
    const themeId = effective === "light" ? editorThemeLight : editorThemeDark;
    // Load the theme if not already loaded, then apply.
    if (isShikiWired()) {
      loadTheme(themeId).then((id) => {
        monaco.editor.setTheme(id);
      });
    } else {
      monaco.editor.setTheme(themeFor(effective, editorThemeDark, editorThemeLight));
    }
  }, [effective, editorThemeDark, editorThemeLight]);

  // Once Shiki finishes loading, switch to its theme and re-tokenize.
  useEffect(() => {
    if (isShikiWired()) {
      if (editorRef.current) {
        const themeId = effective === "light" ? editorThemeLight : editorThemeDark;
        loadTheme(themeId).then((id) => {
          monaco.editor.setTheme(id);
        });
      }
      return;
    }
    initShikiMonaco().then(() => {
      if (editorRef.current) {
        const themeId = effective === "light" ? editorThemeLight : editorThemeDark;
        loadTheme(themeId).then((id) => {
          monaco.editor.setTheme(id);
        });
      }
    });
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // Editor options that can change at runtime.
  useEffect(() => {
    const editor = editorRef.current;
    if (!editor) return;
    editor.updateOptions({
      readOnly,
      minimap: { enabled: settings.editorMinimap, renderCharacters: false },
      fontSize: settings.editorFontSize,
      wordWrap: settings.editorWordWrap ? "on" : "off",
    });
  }, [readOnly, settings.editorMinimap, settings.editorFontSize, settings.editorWordWrap]);

  // ── LSP Document Synchronization ───────────────────────────────────

  const lspLanguage = languageForPath(path);
  const lspClient = useLspClient(lspLanguage, repoRoot ?? null);
  const lspVersionRef = useRef(1);
  const lspChangeTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Tune Monaco options based on language conventions + LSP capabilities.
  // Re-runs when the language changes (new file opened) or when the LSP
  // client becomes available (server finished initializing).
  useEffect(() => {
    const editor = editorRef.current;
    if (!editor) return;

    const capabilities = lspClient?.getCapabilities() ?? null;
    const derived = deriveEditorOptions(lspLanguage, capabilities);

    // Apply tab size / insert spaces to the model (not the editor options)
    // so they persist across model swaps for the same file.
    const model = editor.getModel();
    if (model) {
      model.updateOptions({
        tabSize: derived.tabSize ?? 2,
        insertSpaces: derived.insertSpaces ?? true,
      });
    }

    // Strip model-level options before passing to editor.updateOptions
    const { tabSize: _ts, insertSpaces: _is, ...editorOpts } = derived;
    void _ts;
    void _is;
    editor.updateOptions(editorOpts);
  }, [lspLanguage, lspClient, path]);

  // On mount / path change: notify server of document open
  useEffect(() => {
    if (!lspClient) return;

    lspVersionRef.current = 1;
    lspClient.documentOpened(fileUri, lspLanguage, lspVersionRef.current, value);

    return () => {
      // On unmount / file close: notify server
      lspClient.documentClosed(fileUri);
      if (lspChangeTimerRef.current) {
        clearTimeout(lspChangeTimerRef.current);
        lspChangeTimerRef.current = null;
      }
    };
  }, [lspClient, fileUri, lspLanguage]); // eslint-disable-line react-hooks/exhaustive-deps

  // On content change (debounced 100ms): notify server
  useEffect(() => {
    if (!lspClient || !editorRef.current) return;

    const editor = editorRef.current;
    const disposable = editor.onDidChangeModelContent(() => {
      if (suppressChangeRef.current) return;

      if (lspChangeTimerRef.current) {
        clearTimeout(lspChangeTimerRef.current);
      }
      lspChangeTimerRef.current = setTimeout(() => {
        lspVersionRef.current++;
        lspClient.documentChanged(fileUri, lspVersionRef.current, editor.getValue());
        lspChangeTimerRef.current = null;
      }, 100);
    });

    return () => disposable.dispose();
  }, [lspClient, fileUri]);

  // On save: notify server (and optionally format before saving)
  const originalOnSave = onSaveRef.current;
  const lspOnSave = useRef((val: string) => {
    if (originalOnSave) originalOnSave(val);
    if (lspClient) lspClient.documentSaved(fileUri, val);
  });
  // Keep the lsp save ref in sync
  useEffect(() => {
    lspOnSave.current = (val: string) => {
      const editor = editorRef.current;
      // Format on save: if enabled and the editor has a formatting provider
      if (settings.lspFormatOnSave && editor) {
        const model = editor.getModel();
        if (model) {
          // Trigger format action then save
          editor.getAction("editor.action.formatDocument")?.run().then(() => {
            const formatted = editor.getValue();
            if (onSaveRef.current) onSaveRef.current(formatted);
            if (lspClient) lspClient.documentSaved(fileUri, formatted);
          }).catch(() => {
            // Formatting failed — save without formatting
            if (onSaveRef.current) onSaveRef.current(val);
            if (lspClient) lspClient.documentSaved(fileUri, val);
          });
          return;
        }
      }
      if (onSaveRef.current) onSaveRef.current(val);
      if (lspClient) lspClient.documentSaved(fileUri, val);
    };
  }, [lspClient, fileUri, settings.lspFormatOnSave]);

  return <div ref={containerRef} className="h-full w-full" />;
}

// ─── Model cache ────────────────────────────────────────────────────

const MODEL_CACHE = new Map<string, monaco.editor.ITextModel>();

function getOrCreateModel(
  cacheKey: string,
  uri: monaco.Uri,
  language: string,
  content: string,
): monaco.editor.ITextModel {
  const existing = MODEL_CACHE.get(cacheKey);
  if (existing && !existing.isDisposed()) {
    if (existing.getLanguageId() !== language) {
      monaco.editor.setModelLanguage(existing, language);
    }
    // Always sync model content with the provided value (disk content).
    // The auto-save ensures edits are written to disk promptly, so
    // the value passed here reflects the latest persisted state.
    if (existing.getValue() !== content) {
      existing.setValue(content);
    }
    return existing;
  }
  const byUri = monaco.editor.getModel(uri);
  if (byUri) {
    MODEL_CACHE.set(cacheKey, byUri);
    if (byUri.getLanguageId() !== language) {
      monaco.editor.setModelLanguage(byUri, language);
    }
    if (byUri.getValue() !== content) {
      byUri.setValue(content);
    }
    return byUri;
  }
  const model = monaco.editor.createModel(content, language, uri);
  MODEL_CACHE.set(cacheKey, model);
  return model;
}
