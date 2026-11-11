/**
 * Quick-attach chips rendered inline above the chat composer.
 *
 * Surfaces the context users reach for most often as one-click chips:
 *   - The file currently open in the editor (if any).
 *   - The worktree diff vs HEAD.
 *   - The last N lines of the most recently active terminal session.
 *   - The last git commit (message + SHA for follow-up work).
 *   - Clipboard text content (when non-empty).
 *
 * Each chip produces a `ChatAttachment` that the parent adds via the
 * composer's existing attachment flow, so they play nicely with the
 * manual @ picker and drag-and-drop.
 *
 * The bar is responsive: on narrow viewports it wraps gracefully and
 * hides lower-priority chips behind a "+N more" overflow indicator.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Clipboard,
  FileCode,
  GitCommit,
  History,
  Loader2,
  Terminal,
} from "lucide-react";
import clsx from "clsx";
import type { ChatAttachment } from "./ChatInput";
import {
  listTerminals,
  subscribe as subscribeTerminals,
  terminalTail,
  type TerminalSession,
} from "../../lib/terminal-registry";
import { rpc } from "../../ipc";
import { publishToast } from "../../lib/toasts";

// ── Types ───────────────────────────────────────────────────────────

interface Props {
  /** Task id — used when fetching the worktree diff from the main process. */
  taskId?: string;
  /**
   * Path of the file currently focused in the editor (if one is open).
   * Passed through verbatim as the attachment value.
   */
  activeEditorFile?: string | null;
  /** Repo-relative label shown on the chip — defaults to the basename. */
  activeEditorLabel?: string | null;
  /** Repo id for fetching git log. */
  repoId?: string | null;
  /** Callback to push an attachment into the composer. */
  onAttach: (attachment: ChatAttachment) => void;
}

// ── Component ───────────────────────────────────────────────────────

export function QuickAttachBar({
  taskId,
  activeEditorFile,
  activeEditorLabel,
  repoId,
  onAttach,
}: Props) {
  const [diffLoading, setDiffLoading] = useState(false);
  const [sessions, setSessions] = useState<TerminalSession[]>(() => listTerminals());
  const [lastCommitLabel, setLastCommitLabel] = useState<string | null>(null);
  const [lastCommitValue, setLastCommitValue] = useState<string | null>(null);
  const [clipboardAvailable, setClipboardAvailable] = useState(false);
  const [overflowCount, setOverflowCount] = useState(0);
  const barRef = useRef<HTMLDivElement>(null);

  // Track active terminal sessions so the chip label matches reality.
  useEffect(() => subscribeTerminals(setSessions), []);

  const mostRecentTerminal = sessions[0] ?? null;

  // Fetch last commit on mount / repo change.
  useEffect(() => {
    if (!repoId) {
      setLastCommitLabel(null);
      setLastCommitValue(null);
      return;
    }
    let cancelled = false;
    rpc
      .gitLog(repoId, { max: 1 })
      .then((result) => {
        if (cancelled) return;
        const commit = result.commits?.[0];
        if (commit) {
          const shortSha = commit.sha.slice(0, 7);
          const subject = commit.subject?.split("\n")[0] ?? "";
          const label = subject.length > 28 ? subject.slice(0, 27) + "…" : subject;
          setLastCommitLabel(`${shortSha} ${label}`);
          setLastCommitValue(
            `Commit: ${commit.sha}\nAuthor: ${commit.author}\nDate: ${commit.date}\n\n${commit.subject}`,
          );
        }
      })
      .catch(() => {
        if (!cancelled) setLastCommitLabel(null);
      });
    return () => {
      cancelled = true;
    };
  }, [repoId]);

  // Check clipboard availability (only text content).
  useEffect(() => {
    let cancelled = false;
    const check = async () => {
      try {
        const text = await navigator.clipboard.readText();
        if (!cancelled) setClipboardAvailable(!!text?.trim());
      } catch {
        if (!cancelled) setClipboardAvailable(false);
      }
    };
    check();
    // Re-check when the window regains focus (user may have copied something).
    const onFocus = () => check();
    window.addEventListener("focus", onFocus);
    return () => {
      cancelled = true;
      window.removeEventListener("focus", onFocus);
    };
  }, []);

  // Responsive overflow detection.
  useEffect(() => {
    const bar = barRef.current;
    if (!bar) return;
    const observer = new ResizeObserver(() => {
      // Count chips that overflow past the bar's visible width.
      const chips = bar.querySelectorAll("[data-chip]");
      let hidden = 0;
      const barRect = bar.getBoundingClientRect();
      chips.forEach((chip) => {
        const rect = chip.getBoundingClientRect();
        if (rect.right > barRect.right + 4) hidden++;
      });
      setOverflowCount(hidden);
    });
    observer.observe(bar);
    return () => observer.disconnect();
  }, []);

  const attachActiveFile = useCallback(() => {
    if (!activeEditorFile) return;
    const label = activeEditorLabel || fileBasename(activeEditorFile);
    onAttach({
      id: `quick-file-${Date.now()}`,
      kind: "file",
      value: activeEditorFile,
      label,
    });
  }, [activeEditorFile, activeEditorLabel, onAttach]);

  const attachDiff = useCallback(async () => {
    if (diffLoading) return;
    setDiffLoading(true);
    try {
      const result = await (window as any).crab?.orchestrator?.getWorktreeDiff({
        taskId,
      });
      if (!result?.success) {
        publishToast({
          kind: "error",
          title: "Couldn't read diff",
          body: result?.error ?? "git diff failed",
        });
        return;
      }
      const body = String(result.diff ?? "").trim();
      if (!body) {
        publishToast({ kind: "info", title: "No changes to attach" });
        return;
      }
      const stat = String(result.stat ?? "").trim().split("\n").at(-1) ?? "diff";
      onAttach({
        id: `quick-diff-${Date.now()}`,
        kind: "document",
        value: result.truncated
          ? `# git diff HEAD (truncated at ~200KB)\n\n${body}`
          : `# git diff HEAD\n\n${body}`,
        label: `diff · ${stat}`,
      });
    } catch (err: any) {
      publishToast({ kind: "error", title: "Attach diff failed", body: err?.message });
    } finally {
      setDiffLoading(false);
    }
  }, [diffLoading, onAttach, taskId]);

  const attachTerminalTail = useCallback(() => {
    if (!mostRecentTerminal) return;
    const tail = terminalTail(mostRecentTerminal.id, 12 * 1024);
    if (!tail.trim()) {
      publishToast({
        kind: "info",
        title: "Nothing to attach",
        body: `${mostRecentTerminal.label} has no recent output yet.`,
      });
      return;
    }
    onAttach({
      id: `quick-term-${Date.now()}`,
      kind: "terminal",
      value: tail,
      label: `${shortLabel(mostRecentTerminal.label)} tail`,
    });
  }, [mostRecentTerminal, onAttach]);

  const attachLastCommit = useCallback(() => {
    if (!lastCommitValue) return;
    onAttach({
      id: `quick-commit-${Date.now()}`,
      kind: "commit",
      value: lastCommitValue,
      label: lastCommitLabel ?? "last commit",
    });
  }, [lastCommitLabel, lastCommitValue, onAttach]);

  const attachClipboard = useCallback(async () => {
    try {
      const text = await navigator.clipboard.readText();
      if (!text?.trim()) {
        publishToast({ kind: "info", title: "Clipboard is empty" });
        return;
      }
      const preview = text.trim().split("\n")[0];
      const label = preview.length > 24 ? preview.slice(0, 23) + "…" : preview;
      onAttach({
        id: `quick-clip-${Date.now()}`,
        kind: "document",
        value: `# Clipboard content\n\n${text}`,
        label: `${label}`,
      });
    } catch {
      publishToast({ kind: "error", title: "Clipboard access denied" });
    }
  }, [onAttach]);

  // Collect available chips for rendering.
  const chips = useMemo(() => {
    const result: ChipDef[] = [];
    if (activeEditorFile) {
      result.push({
        key: "file",
        icon: <FileCode size={10} />,
        label: activeEditorLabel || fileBasename(activeEditorFile),
        title: `Attach ${activeEditorFile}`,
        onClick: attachActiveFile,
        priority: 1,
      });
    }
    if (taskId) {
      result.push({
        key: "diff",
        icon: diffLoading ? <Loader2 size={10} className="animate-spin" /> : <GitCommit size={10} />,
        label: "Diff vs HEAD",
        title: "Attach working-tree changes as a diff",
        onClick: attachDiff,
        disabled: diffLoading,
        priority: 2,
      });
    }
    if (mostRecentTerminal) {
      result.push({
        key: "terminal",
        icon: <Terminal size={10} />,
        label: `${shortLabel(mostRecentTerminal.label)} tail`,
        title: `Attach last output from ${mostRecentTerminal.label}`,
        onClick: attachTerminalTail,
        priority: 3,
      });
    }
    if (lastCommitValue) {
      result.push({
        key: "commit",
        icon: <History size={10} />,
        label: lastCommitLabel ?? "last commit",
        title: "Attach the most recent commit message and SHA",
        onClick: attachLastCommit,
        priority: 4,
      });
    }
    if (clipboardAvailable) {
      result.push({
        key: "clipboard",
        icon: <Clipboard size={10} />,
        label: "Clipboard",
        title: "Attach clipboard text content",
        onClick: attachClipboard,
        priority: 5,
      });
    }
    return result.sort((a, b) => a.priority - b.priority);
  }, [
    activeEditorFile,
    activeEditorLabel,
    attachActiveFile,
    taskId,
    diffLoading,
    attachDiff,
    mostRecentTerminal,
    attachTerminalTail,
    lastCommitValue,
    lastCommitLabel,
    attachLastCommit,
    clipboardAvailable,
    attachClipboard,
  ]);

  // Hide entirely when nothing would be attachable.
  if (chips.length === 0) return null;

  return (
    <div
      ref={barRef}
      className="flex items-center gap-1.5 overflow-hidden px-3 pt-1.5 text-[10px]"
      role="toolbar"
      aria-label="Quick attachments"
    >
      <span className="shrink-0 text-muted-foreground">Attach:</span>

      {chips.map((chip) => (
        <Chip
          key={chip.key}
          icon={chip.icon}
          label={chip.label}
          title={chip.title}
          onClick={chip.onClick}
          disabled={chip.disabled}
        />
      ))}

      {overflowCount > 0 && (
        <span className="shrink-0 text-[9px] text-muted-foreground/70">
          +{overflowCount} more
        </span>
      )}
    </div>
  );
}

// ── Types (internal) ────────────────────────────────────────────────

interface ChipDef {
  key: string;
  icon: React.ReactNode;
  label: string;
  title?: string;
  onClick: () => void;
  disabled?: boolean;
  priority: number;
}

// ── Helpers ─────────────────────────────────────────────────────────

function fileBasename(p: string): string {
  const parts = p.split(/[\\/]/);
  return parts[parts.length - 1] || p;
}

function shortLabel(label: string, max = 16): string {
  return label.length > max ? label.slice(0, max - 1) + "…" : label;
}

function Chip({
  icon,
  label,
  title,
  onClick,
  disabled,
}: {
  icon: React.ReactNode;
  label: string;
  title?: string;
  onClick: () => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      data-chip
      onClick={onClick}
      disabled={disabled}
      title={title}
      className={clsx(
        "inline-flex shrink-0 items-center gap-1 rounded-full border px-2 py-0.5 text-[10px] transition",
        "border-border bg-card text-foreground hover:border-primary/50 hover:bg-primary/10",
        "disabled:opacity-50 disabled:cursor-not-allowed",
        "max-w-[160px]",
      )}
    >
      {icon}
      <span className="truncate">{label}</span>
    </button>
  );
}
