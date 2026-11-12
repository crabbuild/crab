import { Button } from "@/components/ui/button";
/**
 * Specialized renderer for Edit / Write / FileDelete tool calls.
 *
 * Routes by the Codex `kind` field on `input` (add | update | delete)
 * with a fallback to legacy heuristics for adapters that don't set it:
 *
 *   - add    → new-file Write. Body rendered as syntax-highlighted
 *              code via `<ShikiPreview />`.
 *   - update → patch fragment or full unified diff. Body normalised
 *              and rendered via `<UnifiedDiff />` (lazy-loaded shim
 *              over SafePatchDiff).
 *   - delete → file removal. Header tinted red, body shows the lost
 *              content as a deletion-only diff.
 *
 * Status (`in_progress` | `completed` | `failed` | `declined`) is
 * surfaced as a left border colour and an inline badge so users see
 * the transition without expanding the card.
 *
 * Agent-agnostic: works with any adapter that provides:
 *   - input.file_path / input.path
 *   - input.kind ∈ {"add", "update", "delete"} (preferred)
 *   - input.status (optional; defaults to "completed")
 *   - input.old_string + input.new_string (Edit, Claude-style)
 *   - input.content / input.diff (Write or partial diff body)
 */

import clsx from "clsx";
import {
  Check,
  ChevronDown,
  ChevronRight,
  FileMinus2,
  FilePen,
  FilePlus2,
  Loader2,
  ShieldOff,
  Undo2,
  XCircle,
} from "lucide-react";
import { useMemo, useState } from "react";
import {
  extractAddedFileContent,
  isPartialDiff,
  isUnifiedDiff,
  synthesizeUnifiedDiff,
} from "../../../lib/codex-diff";
import { publishToast } from "../../../lib/toasts";
import { parseDiffStats } from "../../../lib/unified-diff-stats";
import { UnifiedDiff } from "../UnifiedDiff";
import { useHunkActions } from "./HunkActionsContext";
import { ShikiPreview } from "./ShikiPreview";
import type { ToolCardRendererProps } from "./types";

// ── Component ───────────────────────────────────────────────────────

type EditKind = "add" | "update" | "delete";
type EditStatus = "in_progress" | "completed" | "failed" | "declined" | "updated";

export function EditCard({
  event,
  defaultExpanded = false,
  collapsed: collapsedProp,
  onToggleCollapse,
}: ToolCardRendererProps) {
  const [expandedLocal, setExpandedLocal] = useState(defaultExpanded);
  const [hunkState, setHunkState] = useState<"idle" | "applying" | "applied" | "reverting" | "reverted" | "error">("idle");
  const [hunkError, setHunkError] = useState<string | null>(null);

  // Collapse state: external prop takes precedence over local state.
  const collapsed = collapsedProp ?? !expandedLocal;
  const toggleCollapse = () => {
    if (onToggleCollapse) {
      onToggleCollapse(!collapsed);
    } else {
      setExpandedLocal(collapsed);
    }
  };

  const input = event.input as Record<string, unknown> | undefined;
  const filePath = String(input?.file_path ?? input?.path ?? "");
  const oldString = String(input?.old_string ?? "");
  const newString = String(input?.new_string ?? "");
  const content = String(input?.content ?? "");
  const diffField = String(input?.diff ?? event.detail ?? "");

  // Resolve kind. Codex sets `input.kind` explicitly. For Claude-style
  // Edit, fall back to derivation (no old_string + new_string ⇒ add;
  // both ⇒ update; only old_string ⇒ delete is rare but handled).
  const explicitKind = (input?.kind as EditKind | undefined) ?? null;
  const kind: EditKind = explicitKind ?? deriveKind(event.tool, oldString, newString, content);
  const status = (input?.status as EditStatus | undefined) ?? "completed";

  // Pick the body text to render. Order matters: explicit fields
  // first, then the `diff` (Codex update fragment), then `content`
  // (Codex add or Claude Write body).
  const body: string = useMemo(() => {
    if (oldString || newString) {
      return diffFromOldNew(oldString, newString);
    }
    return diffField || content;
  }, [oldString, newString, diffField, content]);

  // Short filename for display.
  const shortPath = filePath.split("/").pop() ?? filePath;

  const { Icon, label, accent } = renderingHints(kind, event.tool);
  const tone = statusTone(status);

  // Compute additions/deletions using parseDiffStats for the badge.
  const stats = useMemo(() => {
    if (!body) return { added: 0, removed: 0, show: false };
    if (kind === "add") {
      const lines = body.split("\n").filter((l) => l !== "").length;
      return { added: lines, removed: 0, show: true };
    }
    const { added, removed } = parseDiffStats(body);
    return { added, removed, show: added > 0 || removed > 0 };
  }, [body, kind]);

  // Determine if the diff is empty for non-add kinds.
  const diffEmpty = !body && kind !== "add";
  const diffEmptyUpdate = kind !== "add" && body !== "" && !stats.show;

  const hunkContext = useHunkActions();
  const canManipulateHunk = !!hunkContext?.taskId
    && !!filePath
    && kind !== "delete"
    && (event.tool === "Write" || event.tool === "Edit" || event.tool === "MultiEdit"
        ? !!(content || diffField || newString)
        : !!(oldString || newString));

  const applyHunk = async () => {
    if (!hunkContext) return;
    setHunkState("applying");
    setHunkError(null);
    try {
      const patch = {
        filePath,
        oldString: kind === "add" ? "" : oldString,
        newString: kind === "add" ? content : newString,
      };
      const result = await (window as any).crab?.orchestrator?.applyHunk(hunkContext.taskId, patch);
      if (result?.success) {
        setHunkState("applied");
        publishToast({ kind: "success", title: "Hunk applied", body: shortPath });
      } else {
        setHunkState("error");
        setHunkError(result?.error ?? "Apply failed");
        publishToast({ kind: "error", title: "Apply failed", body: result?.error });
      }
    } catch (err: any) {
      setHunkState("error");
      setHunkError(err?.message ?? String(err));
    }
  };

  const revertHunkAction = async () => {
    if (!hunkContext) return;
    setHunkState("reverting");
    setHunkError(null);
    try {
      const patch = {
        filePath,
        oldString: kind === "add" ? "" : oldString,
        newString: kind === "add" ? content : newString,
      };
      const result = await (window as any).crab?.orchestrator?.revertHunk(hunkContext.taskId, patch);
      if (result?.success) {
        setHunkState("reverted");
        publishToast({ kind: "success", title: "Hunk reverted", body: shortPath });
      } else {
        setHunkState("error");
        setHunkError(result?.error ?? "Revert failed");
        publishToast({ kind: "error", title: "Revert failed", body: result?.error });
      }
    } catch (err: any) {
      setHunkState("error");
      setHunkError(err?.message ?? String(err));
    }
  };

  // ── Collapsed one-liner branch ──────────────────────────────────

  if (collapsed) {
    return (
      <div className={clsx("rounded-md border overflow-hidden transition-colors", tone.border, tone.bg)}>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={toggleCollapse}
          className="justify-start flex w-full items-center gap-2 px-2.5 py-1.5 text-left hover:bg-accent transition cursor-pointer"
          aria-expanded="false"
        >
          <Icon size={11} className={clsx("shrink-0", accent)} />

          {kind === "update" && (
            <>
              <span className="text-[10px] text-foreground/90 font-medium truncate">
                Edited {shortPath || "(no path)"}
              </span>
              {stats.show && (
                <span className="flex items-center gap-1 shrink-0">
                  <span className="text-[9px] font-mono text-success">
                    +{stats.added}
                  </span>
                  <span className="text-[9px] font-mono text-destructive">
                    −{stats.removed}
                  </span>
                </span>
              )}
              {/* Empty diff, no badge — quietly styled one-liner */}
              {(diffEmpty || diffEmptyUpdate) && !stats.show && (
                <span className="text-[9px] text-muted-foreground/60 italic" />
              )}
            </>
          )}

          {kind === "add" && (
            <>
              <span className="text-[10px] text-foreground/90 font-medium truncate">
                Created {shortPath || "(no path)"}
              </span>
              {stats.show && (
                <span className="flex items-center gap-1 shrink-0">
                  <span className="text-[9px] font-mono text-success">
                    + {stats.added} lines
                  </span>
                </span>
              )}
            </>
          )}

          {kind === "delete" && (
            <>
              <span className="text-[10px] text-foreground/90 font-medium truncate line-through">
                Deleted {shortPath || "(no path)"}
              </span>
              {stats.show && (
                <span className="flex items-center gap-1 shrink-0">
                  <span className="text-[9px] font-mono text-destructive">
                    − {stats.removed} lines
                  </span>
                </span>
              )}
            </>
          )}

          <span className="flex-1" />
          <StatusBadge status={status} />
          <ChevronRight size={10} className="shrink-0 text-muted-foreground/60" />
        </Button>
      </div>
    );
  }

  // ── Expanded branch ─────────────────────────────────────────────

  return (
    <div className={clsx("rounded-md border overflow-hidden transition-colors", tone.border, tone.bg)}>
      {/* Header */}
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={toggleCollapse}
        className="justify-start flex w-full items-center gap-2 px-2.5 py-1.5 text-left hover:bg-accent transition cursor-pointer"
        aria-expanded="true"
      >
        <Icon size={11} className={clsx("shrink-0", accent)} />
        <span className={clsx("shrink-0 text-[10px] font-medium", accent)}>
          {label}
        </span>

        {/* File path */}
        <span className="min-w-0 flex-1 flex items-baseline gap-1 truncate">
          <span className={clsx(
            "text-[10px] text-foreground/90 font-medium truncate",
            kind === "delete" && "line-through",
          )}>
            {shortPath || "(no path)"}
          </span>
        </span>

        {/* Change stats */}
        {stats.show && (
          <span className="flex items-center gap-1.5 shrink-0">
            {stats.added > 0 && (
              <span className="text-[9px] font-mono text-success">
                +{stats.added}
              </span>
            )}
            {stats.removed > 0 && (
              <span className="text-[9px] font-mono text-destructive">
                -{stats.removed}
              </span>
            )}
          </span>
        )}

        {/* Status badge */}
        <StatusBadge status={status} />

        {/* Hunk apply / revert — visible only when expanded */}
        {canManipulateHunk && (
          <span className="flex items-center gap-1 shrink-0" onClick={(e) => e.stopPropagation()}>
            {hunkState === "applied" ? (
              <span className="inline-flex items-center gap-0.5 text-[9px] text-success">
                <Check size={9} /> Applied
              </span>
            ) : hunkState === "reverted" ? (
              <span className="inline-flex items-center gap-0.5 text-[9px] text-warning">
                <Undo2 size={9} /> Reverted
              </span>
            ) : (
              <>
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  disabled={hunkState === "applying" || hunkState === "reverting"}
                  onClick={() => void applyHunk()}
                  className="inline-flex items-center gap-0.5 rounded border border-success/40 bg-success/10 px-1 py-0.5 text-[9px] text-success hover:bg-success/20 transition disabled:opacity-50"
                  title="Apply this edit to disk"
                >
                  {hunkState === "applying" ? (
                    <Loader2 size={9} className="animate-spin" />
                  ) : (
                    <Check size={9} />
                  )}
                  Apply
                </Button>
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  disabled={hunkState === "applying" || hunkState === "reverting"}
                  onClick={() => void revertHunkAction()}
                  className="inline-flex items-center gap-0.5 rounded border border-destructive/40 bg-destructive/10 px-1 py-0.5 text-[9px] text-destructive hover:bg-destructive/20 transition disabled:opacity-50"
                  title="Revert this edit"
                >
                  {hunkState === "reverting" ? (
                    <Loader2 size={9} className="animate-spin" />
                  ) : (
                    <Undo2 size={9} />
                  )}
                  Revert
                </Button>
              </>
            )}
          </span>
        )}

        <ChevronDown size={10} className="shrink-0 text-muted-foreground/60" />
      </Button>

      {/* Hunk error, if any */}
      {hunkError && (
        <div className="border-t border-destructive/30 bg-destructive/5 px-2.5 py-1 text-[9px] text-destructive">
          {hunkError}
        </div>
      )}

      {/* Expanded body */}
      <ExpandedBody
        kind={kind}
        status={status}
        body={body}
        filePath={filePath}
        newFileContent={kind === "add" ? extractAddedFileContent(body) : ""}
      />
    </div>
  );
}

// ── Body renderer ───────────────────────────────────────────────────

function ExpandedBody({
  kind,
  status,
  body,
  filePath,
  newFileContent,
}: {
  kind: EditKind;
  status: EditStatus;
  body: string;
  filePath: string;
  newFileContent: string;
}) {
  // No content yet (item.started before any delta) — show a spinner.
  if (!body && status === "in_progress") {
    return (
      <div className="border-t border-border/30 px-2.5 py-2 flex items-center gap-2 text-[10px] text-muted-foreground">
        <Loader2 size={10} className="animate-spin" />
        Waiting for patch…
      </div>
    );
  }

  // Empty diff for non-add kinds: quietly styled one-liner, no legacy
  // "No displayable changes" placeholder.
  if (!body && kind !== "add") {
    return (
      <div className="border-t border-border/30 px-2.5 py-2 text-[10px] text-muted-foreground/60 italic">
        Edited {filePath.split("/").pop() ?? filePath}
      </div>
    );
  }

  if (!body) {
    return (
      <div className="border-t border-border/30 px-2.5 py-2 text-[10px] text-muted-foreground italic">
        (no body)
      </div>
    );
  }

  // New file: render the body as syntax-highlighted code.
  if (kind === "add" && newFileContent) {
    return (
      <div className="border-t border-border/30 max-h-[320px] overflow-auto">
        <ShikiPreview code={newFileContent} path={filePath} />
      </div>
    );
  }

  // Update or delete: render via UnifiedDiff (lazy-loaded).
  const synth = isUnifiedDiff(body) || isPartialDiff(body) || kind === "delete"
    ? synthesizeUnifiedDiff(body, filePath, kind)
    : null;

  if (synth) {
    return (
      <div className="border-t border-border/30 max-h-[320px] overflow-auto bg-background/30">
        <UnifiedDiff patch={synth} />
      </div>
    );
  }

  // Last-resort fallback for non-diff non-add bodies (e.g., a raw
  // Edit with old_string == new_string == "" and only `content`).
  return (
    <div className="border-t border-border/30 max-h-[320px] overflow-auto">
      <ShikiPreview code={body} path={filePath} />
    </div>
  );
}

// ── Helpers ─────────────────────────────────────────────────────────

function deriveKind(tool: string | undefined, oldString: string, newString: string, content: string): EditKind {
  if (tool === "FileDelete") return "delete";
  if (tool === "Write") return "add";
  if (oldString && !newString) return "delete";
  if (!oldString && newString) return "add";
  if (oldString && newString) return "update";
  if (content) return "add";
  return "update";
}

function diffFromOldNew(oldString: string, newString: string): string {
  if (!oldString && !newString) return "";
  const minus = oldString ? oldString.split("\n").map((l) => "-" + l).join("\n") : "";
  const plus = newString ? newString.split("\n").map((l) => "+" + l).join("\n") : "";
  if (minus && plus) return `${minus}\n${plus}`;
  return minus || plus;
}

function renderingHints(kind: EditKind, tool: string | undefined): {
  Icon: typeof FilePen;
  label: string;
  accent: string;
} {
  if (kind === "add") return { Icon: FilePlus2, label: "Write", accent: "text-success/80" };
  if (kind === "delete") return { Icon: FileMinus2, label: "Delete", accent: "text-destructive/80" };
  return { Icon: FilePen, label: tool === "MultiEdit" ? "MultiEdit" : "Edit", accent: "text-muted-foreground" };
}

function statusTone(status: EditStatus): { border: string; bg: string } {
  switch (status) {
    case "in_progress":
    case "updated":
      return { border: "border-info/30", bg: "bg-info/5" };
    case "failed":
      return { border: "border-destructive/40", bg: "bg-destructive/5" };
    case "declined":
      return { border: "border-warning/40", bg: "bg-warning/5" };
    case "completed":
    default:
      return { border: "border-border/40", bg: "" };
  }
}

function StatusBadge({ status }: { status: EditStatus }) {
  if (status === "completed") return null;
  if (status === "in_progress" || status === "updated") {
    return (
      <span className="shrink-0 inline-flex items-center gap-0.5 rounded px-1 py-0.5 text-[8px] font-medium text-info border border-info/30 bg-info/10">
        <Loader2 size={9} className="animate-spin" /> running
      </span>
    );
  }
  if (status === "failed") {
    return (
      <span className="shrink-0 inline-flex items-center gap-0.5 rounded px-1 py-0.5 text-[8px] font-medium text-destructive border border-destructive/30 bg-destructive/10">
        <XCircle size={9} /> failed
      </span>
    );
  }
  if (status === "declined") {
    return (
      <span className="shrink-0 inline-flex items-center gap-0.5 rounded px-1 py-0.5 text-[8px] font-medium text-warning border border-warning/30 bg-warning/10">
        <ShieldOff size={9} /> declined
      </span>
    );
  }
  return null;
}
