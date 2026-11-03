/**
 * ReviewNavigatorContext — manages keyboard navigation state for the PR Files tab.
 *
 * Tracks which element (line or thread) is currently focused for keyboard nav,
 * controls the shortcut overlay visibility, and registers a keydown listener
 * on the container element. Single-key shortcuts are suppressed when a text
 * input, textarea, or contenteditable element has focus.
 *
 * The provider accepts threads, files, and callbacks to implement cross-file
 * navigation (n/p for threads, ]/[ for unviewed files, c for composer).
 *
 * Usage:
 *   <ReviewNavigatorProvider
 *     threads={threads}
 *     files={files}
 *     isViewed={isViewed}
 *     onOpenComposer={handleOpenComposer}
 *   >
 *     …Files tab content…
 *   </ReviewNavigatorProvider>
 *
 *   const { focus, shortcutsVisible, toggleShortcuts } = useReviewNavigator();
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

import type { FocusTarget, ReviewThread } from "./types";
import type { PrChangedFile } from "../ipc";
import { publishToast } from "../lib/toasts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface ReviewNavigatorState {
  focus: FocusTarget;
  shortcutsVisible: boolean;
}

export interface ReviewNavigatorActions {
  nextThread(): void;
  prevThread(): void;
  nextFile(): void;
  prevFile(): void;
  focusLine(path: string, line: number): void;
  openComposer(): void;
  toggleShortcuts(): void;
}

export type ReviewNavigatorContextValue = ReviewNavigatorState & ReviewNavigatorActions;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Single-key shortcuts that must be suppressed when a text input has focus. */
const SINGLE_KEY_SHORTCUTS = new Set(["j", "k", "n", "p", "c", "]", "[", "?"]);

/**
 * Returns true when the active element is a text input, textarea, or
 * contenteditable — meaning single-key shortcuts should be suppressed.
 */
function isTextInputFocused(event: KeyboardEvent): boolean {
  const target = event.target;
  if (!(target instanceof HTMLElement)) return false;

  const tagName = target.tagName.toLowerCase();
  if (tagName === "textarea") return true;
  if (tagName === "input") {
    const inputType = (target as HTMLInputElement).type.toLowerCase();
    // Only suppress for text-like inputs, not checkboxes/radios/buttons.
    const textTypes = new Set([
      "text",
      "search",
      "url",
      "tel",
      "email",
      "password",
      "number",
    ]);
    return textTypes.has(inputType);
  }
  if (target.isContentEditable) return true;

  return false;
}

/**
 * Returns true if the event uses a modifier key (Cmd/Ctrl/Alt/Shift+key combos
 * beyond just Shift). Modifier-based shortcuts remain active even in text inputs.
 */
function hasModifier(event: KeyboardEvent): boolean {
  return event.metaKey || event.ctrlKey || event.altKey;
}

/**
 * Collects all unresolved threads sorted by file order then line number.
 * Returns a flat list with file index for cross-file navigation.
 */
function getUnresolvedThreads(
  threads: ReviewThread[],
  files: PrChangedFile[],
): ReviewThread[] {
  const fileOrder = new Map<string, number>();
  files.forEach((f, i) => fileOrder.set(f.path, i));

  return threads
    .filter((t) => !t.resolved)
    .sort((a, b) => {
      const fileA = fileOrder.get(a.path) ?? Infinity;
      const fileB = fileOrder.get(b.path) ?? Infinity;
      if (fileA !== fileB) return fileA - fileB;
      return a.line - b.line;
    });
}

/**
 * Scrolls an element matching the given focus target into view.
 * Uses data attributes to locate the element in the DOM.
 */
function scrollFocusIntoView(container: HTMLElement | null, target: FocusTarget): void {
  if (!container || !target) return;

  let selector: string;
  if (target.type === "line") {
    selector = `[data-nav-path="${CSS.escape(target.path)}"][data-nav-line="${target.line}"]`;
  } else {
    selector = `[data-nav-thread="${CSS.escape(target.threadId)}"]`;
  }

  const el = container.querySelector(selector);
  if (el) {
    el.scrollIntoView({ behavior: "smooth", block: "center" });
  }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

const ReviewNavigatorContext = createContext<ReviewNavigatorContextValue | null>(null);

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

export interface ReviewNavigatorProviderProps {
  children: ReactNode;
  /** All review threads for the PR (used for n/p navigation). */
  threads?: ReviewThread[];
  /** All changed files in the PR (used for ]/[ navigation and ordering). */
  files?: PrChangedFile[];
  /** Returns whether a file path has been marked as viewed. */
  isViewed?(path: string): boolean;
  /** Callback to open the inline comment composer on a specific file + line. */
  onOpenComposer?(path: string, line: number): void;
}

export function ReviewNavigatorProvider({
  children,
  threads = [],
  files = [],
  isViewed,
  onOpenComposer,
}: ReviewNavigatorProviderProps) {
  const [focus, setFocus] = useState<FocusTarget>(null);
  const [shortcutsVisible, setShortcutsVisible] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);

  // Keep refs for use in callbacks without stale closures.
  const focusRef = useRef<FocusTarget>(focus);
  focusRef.current = focus;

  const threadsRef = useRef(threads);
  threadsRef.current = threads;

  const filesRef = useRef(files);
  filesRef.current = files;

  const isViewedRef = useRef(isViewed);
  isViewedRef.current = isViewed;

  const onOpenComposerRef = useRef(onOpenComposer);
  onOpenComposerRef.current = onOpenComposer;

  // --- Action implementations ---

  const focusLine = useCallback((path: string, line: number) => {
    const target: FocusTarget = { type: "line", path, line };
    setFocus(target);
    scrollFocusIntoView(containerRef.current, target);
  }, []);

  const nextLine = useCallback(() => {
    const current = focusRef.current;
    const currentFiles = filesRef.current;

    if (!current) {
      // No focus yet — start at line 1 of the first file.
      if (currentFiles.length > 0) {
        focusLine(currentFiles[0].path, 1);
      }
      return;
    }

    if (current.type === "line") {
      focusLine(current.path, current.line + 1);
    } else {
      // Focus is on a thread — move to the next line after the thread's line.
      const thread = threadsRef.current.find((t) => t.threadId === current.threadId);
      if (thread) {
        focusLine(thread.path, thread.line + 1);
      }
    }
  }, [focusLine]);

  const prevLine = useCallback(() => {
    const current = focusRef.current;
    const currentFiles = filesRef.current;

    if (!current) {
      // No focus yet — start at line 1 of the first file.
      if (currentFiles.length > 0) {
        focusLine(currentFiles[0].path, 1);
      }
      return;
    }

    if (current.type === "line") {
      const newLine = Math.max(1, current.line - 1);
      focusLine(current.path, newLine);
    } else {
      // Focus is on a thread — move to the line before the thread's line.
      const thread = threadsRef.current.find((t) => t.threadId === current.threadId);
      if (thread) {
        focusLine(thread.path, Math.max(1, thread.line - 1));
      }
    }
  }, [focusLine]);

  const nextThread = useCallback(() => {
    const current = focusRef.current;
    const currentThreads = threadsRef.current;
    const currentFiles = filesRef.current;

    const unresolved = getUnresolvedThreads(currentThreads, currentFiles);

    if (unresolved.length === 0) {
      publishToast({ kind: "info", title: "No unresolved threads remaining" });
      return;
    }

    // Find the next unresolved thread after the current focus position.
    let nextIdx = 0;

    if (current) {
      const fileOrder = new Map<string, number>();
      currentFiles.forEach((f, i) => fileOrder.set(f.path, i));

      const currentPath = current.type === "line" ? current.path : current.path;
      const currentLine =
        current.type === "line"
          ? current.line
          : currentThreads.find((t) => t.threadId === current.threadId)?.line ?? 0;
      const currentFileIdx = fileOrder.get(currentPath) ?? -1;

      // Find the first unresolved thread that comes after the current position.
      const afterIdx = unresolved.findIndex((t) => {
        const tFileIdx = fileOrder.get(t.path) ?? Infinity;
        if (tFileIdx > currentFileIdx) return true;
        if (tFileIdx === currentFileIdx && t.line > currentLine) return true;
        return false;
      });

      // If none found after current position, wrap to the first one.
      nextIdx = afterIdx >= 0 ? afterIdx : 0;
    }

    const target = unresolved[nextIdx];
    const newFocus: FocusTarget = { type: "thread", path: target.path, threadId: target.threadId };
    setFocus(newFocus);
    scrollFocusIntoView(containerRef.current, newFocus);
  }, []);

  const prevThread = useCallback(() => {
    const current = focusRef.current;
    const currentThreads = threadsRef.current;
    const currentFiles = filesRef.current;

    const unresolved = getUnresolvedThreads(currentThreads, currentFiles);

    if (unresolved.length === 0) {
      publishToast({ kind: "info", title: "No unresolved threads remaining" });
      return;
    }

    // Find the previous unresolved thread before the current focus position.
    let prevIdx = unresolved.length - 1;

    if (current) {
      const fileOrder = new Map<string, number>();
      currentFiles.forEach((f, i) => fileOrder.set(f.path, i));

      const currentPath = current.type === "line" ? current.path : current.path;
      const currentLine =
        current.type === "line"
          ? current.line
          : currentThreads.find((t) => t.threadId === current.threadId)?.line ?? 0;
      const currentFileIdx = fileOrder.get(currentPath) ?? -1;

      // Find the last unresolved thread that comes before the current position.
      let foundIdx = -1;
      for (let i = unresolved.length - 1; i >= 0; i--) {
        const t = unresolved[i];
        const tFileIdx = fileOrder.get(t.path) ?? Infinity;
        if (tFileIdx < currentFileIdx) {
          foundIdx = i;
          break;
        }
        if (tFileIdx === currentFileIdx && t.line < currentLine) {
          foundIdx = i;
          break;
        }
      }

      // If none found before current position, wrap to the last one.
      prevIdx = foundIdx >= 0 ? foundIdx : unresolved.length - 1;
    }

    const target = unresolved[prevIdx];
    const newFocus: FocusTarget = { type: "thread", path: target.path, threadId: target.threadId };
    setFocus(newFocus);
    scrollFocusIntoView(containerRef.current, newFocus);
  }, []);

  const nextFile = useCallback(() => {
    const currentFiles = filesRef.current;
    const checkViewed = isViewedRef.current;

    if (!checkViewed || currentFiles.length === 0) return;

    const current = focusRef.current;
    const currentPath = current?.path ?? "";
    const currentIdx = currentFiles.findIndex((f) => f.path === currentPath);

    // Search forward from current position, wrapping around.
    const len = currentFiles.length;
    for (let offset = 1; offset <= len; offset++) {
      const idx = (currentIdx + offset) % len;
      const file = currentFiles[idx];
      if (!checkViewed(file.path)) {
        focusLine(file.path, 1);
        return;
      }
    }
    // All files are viewed — no-op.
  }, [focusLine]);

  const prevFile = useCallback(() => {
    const currentFiles = filesRef.current;
    const checkViewed = isViewedRef.current;

    if (!checkViewed || currentFiles.length === 0) return;

    const current = focusRef.current;
    const currentPath = current?.path ?? "";
    const currentIdx = currentFiles.findIndex((f) => f.path === currentPath);
    const startIdx = currentIdx >= 0 ? currentIdx : 0;

    // Search backward from current position, wrapping around.
    const len = currentFiles.length;
    for (let offset = 1; offset <= len; offset++) {
      const idx = (startIdx - offset + len) % len;
      const file = currentFiles[idx];
      if (!checkViewed(file.path)) {
        focusLine(file.path, 1);
        return;
      }
    }
    // All files are viewed — no-op.
  }, [focusLine]);

  const openComposer = useCallback(() => {
    const current = focusRef.current;
    const callback = onOpenComposerRef.current;

    if (!current || !callback) return;

    if (current.type === "line") {
      callback(current.path, current.line);
    } else {
      // Focus is on a thread — open composer on the thread's line.
      const thread = threadsRef.current.find((t) => t.threadId === current.threadId);
      if (thread) {
        callback(thread.path, thread.line);
      }
    }
  }, []);

  const toggleShortcuts = useCallback(() => {
    setShortcutsVisible((prev) => !prev);
  }, []);

  // --- Keyboard event handler ---

  const handleKeyDown = useCallback(
    (event: KeyboardEvent) => {
      const key = event.key;

      // If a text input has focus, suppress single-key shortcuts but allow
      // modifier-based shortcuts (Cmd+Enter, Ctrl+Enter) to pass through.
      if (isTextInputFocused(event)) {
        if (SINGLE_KEY_SHORTCUTS.has(key) && !hasModifier(event)) {
          return; // Let the text input handle the keystroke normally.
        }
      }

      // Shortcut dispatch.
      switch (key) {
        case "j":
          if (!hasModifier(event)) {
            event.preventDefault();
            nextLine();
          }
          break;

        case "k":
          if (!hasModifier(event)) {
            event.preventDefault();
            prevLine();
          }
          break;

        case "n":
          if (!hasModifier(event)) {
            event.preventDefault();
            nextThread();
          }
          break;

        case "p":
          if (!hasModifier(event)) {
            event.preventDefault();
            prevThread();
          }
          break;

        case "]":
          if (!hasModifier(event)) {
            event.preventDefault();
            nextFile();
          }
          break;

        case "[":
          if (!hasModifier(event)) {
            event.preventDefault();
            prevFile();
          }
          break;

        case "c":
          if (!hasModifier(event)) {
            event.preventDefault();
            openComposer();
          }
          break;

        case "?":
          if (!hasModifier(event)) {
            event.preventDefault();
            setShortcutsVisible((prev) => !prev);
          }
          break;

        default:
          break;
      }
    },
    [nextLine, prevLine, nextThread, prevThread, nextFile, prevFile, openComposer],
  );

  // Register keydown listener on the container element.
  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    container.addEventListener("keydown", handleKeyDown);
    return () => {
      container.removeEventListener("keydown", handleKeyDown);
    };
  }, [handleKeyDown]);

  // --- Context value ---

  const value: ReviewNavigatorContextValue = {
    focus,
    shortcutsVisible,
    nextThread,
    prevThread,
    nextFile,
    prevFile,
    focusLine,
    openComposer,
    toggleShortcuts,
  };

  return (
    <ReviewNavigatorContext.Provider value={value}>
      <div ref={containerRef} tabIndex={-1} style={{ outline: "none" }}>
        {children}
      </div>
    </ReviewNavigatorContext.Provider>
  );
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

/**
 * Returns the review navigator state and actions.
 *
 * Must be called within a `<ReviewNavigatorProvider>`.
 */
export function useReviewNavigator(): ReviewNavigatorContextValue {
  const ctx = useContext(ReviewNavigatorContext);
  if (!ctx) {
    throw new Error("useReviewNavigator must be used within a ReviewNavigatorProvider");
  }
  return ctx;
}
