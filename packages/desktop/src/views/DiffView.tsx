import { useEffect, useRef, useState, useCallback } from "react";
import { SafePatchDiff } from "../components/SafePatchDiff";
import { LargeFileDiffView } from "../components/LargeFileDiffView";
import { FileMinus, GitCompareArrows, Loader2 } from "lucide-react";
import { OpenRepo } from "../state";
import { fileDiffCancellable, isCancelledError, type FileComparison, type DiffVersion } from "../ipc";
import { useSetting } from "../settings";
import { useEffectiveTheme } from "../hooks/useEffectiveTheme";
import { diffOptions } from "../lib/diffTheme";
import { NotebookDiffView } from "../components/NotebookDiffView";
import { PatchMinimap } from "../components/PatchMinimap";
import {
  CommentGutter,
  type CommentsByLine,
  type DiffComment,
} from "../components/CommentGutter";

export type { DiffComment, CommentsByLine };

type Comparison = "working" | "unstaged" | "staged";

interface Props {
  repo: OpenRepo;
  path: string;
  /** Optional A/B ref pair; `null` means working tree vs HEAD. */
  base?: string | null;
  head?: string | null;
  /** Optional map of line numbers to comment threads (forge-aware overlay). */
  commentsByLine?: CommentsByLine;
  /** Called when the user submits a new inline comment on a line. */
  onAddComment?: (line: number, body: string) => void;
}

/** Returns true when the file path has a Jupyter notebook extension. */
function isNotebook(filePath: string): boolean {
  return filePath.toLowerCase().endsWith(".ipynb");
}

/**
 * Main-pane diff renderer backed by @pierre/diffs' `PatchDiff`.
 *
 * The agent runs `git diff [base..head] -- <path>` and returns the
 * raw unified patch; we feed that straight into `<PatchDiff patch=/>`
 * which handles parsing, syntax highlighting (via Shiki), and the
 * "expand unchanged" affordance.
 *
 * For `.ipynb` files, routes to `<NotebookDiffView />` which renders
 * a structured cell-level diff instead of raw JSON.
 */
export function DiffView({ repo, path, base, head, commentsByLine, onAddComment }: Props) {
  // Route notebook files to the cell-level diff view
  if (isNotebook(path)) {
    return (
      <div className="flex h-full min-h-0 flex-col bg-gx-bg-sub">
        <DiffHeader path={path} badge="notebook" base={base} head={head} />
        <div className="min-h-0 flex-1 overflow-auto">
          <NotebookDiffView repo={repo} path={path} base={base} head={head} />
        </div>
      </div>
    );
  }

  return (
    <PatchDiffView
      repo={repo}
      path={path}
      base={base}
      head={head}
      commentsByLine={commentsByLine}
      onAddComment={onAddComment}
    />
  );
}

// --- Internal components ---

function DiffHeader({
  path,
  badge,
  base,
  head,
  comparison,
  onComparisonChange,
}: {
  path: string;
  badge: string;
  comparison?: Comparison;
  onComparisonChange?: (value: Comparison) => void;
  base?: string | null;
  head?: string | null;
}) {
  return (
    <div className="flex shrink-0 items-center gap-3 border-b border-gx-border bg-gx-surface px-3 py-1.5 text-[11px] text-gx-muted">
      <GitCompareArrows size={13} className="text-gx-accent" />
      <span className="min-w-0 truncate font-mono text-gx-text" title={path}>{path}</span>
      <span className="chip text-[9px]">{badge}</span>
      <span className="ml-auto">
        {onComparisonChange && comparison ? (
          <select aria-label="Compare file versions" className="rounded border border-gx-border bg-gx-surface px-2 py-1 text-gx-text" value={comparison} onChange={(event) => onComparisonChange(event.target.value as Comparison)}>
            <option value="working">HEAD → Working tree</option>
            <option value="unstaged">Index → Working tree</option>
            <option value="staged">HEAD → Index</option>
          </select>
        ) : base || head ? (
          <>
            {base ?? (head ? "Empty tree" : "HEAD")} … {head ?? "(working tree)"}
          </>
        ) : (
          "Working tree vs HEAD"
        )}
      </span>
    </div>
  );
}

/**
 * Standard patch-based diff view. Extracted so the notebook early-
 * return in `DiffView` doesn't violate React's rules of hooks.
 */
function PatchDiffView({ repo, path, base, head, commentsByLine, onAddComment }: Props) {
  const [result, setResult] = useState<FileComparison | null>(null);
  const patch = result?.kind === "git" ? result.patch : null;
  const largeFileDiff = result?.kind === "large_file" ? result : null;
  const [err, setErr] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);
  const [comparison, setComparison] = useState<Comparison>("working");
  const minimap = useSetting("editorMinimap");
  const effective = useEffectiveTheme();
  const scrollRef = useRef<HTMLDivElement>(null);

  const handleRefresh = useCallback(() => {
    setRefreshKey((k) => k + 1);
  }, []);

  useEffect(() => {
    let cancelFn: (() => void) | null = null;
    let cancelled = false;
    setResult(null);
    setErr(null);

    const attempt = async () => {
      const before: DiffVersion = base ? { kind: "revision", rev: base }
        : head ? { kind: "empty" } : comparison === "unstaged" ? { kind: "index" } : { kind: "revision", rev: "HEAD" };
      const after: DiffVersion = head ? { kind: "revision", rev: head } : comparison === "staged" ? { kind: "index" } : { kind: "working" };
      const { promise, cancel } = fileDiffCancellable(repo.repo_id, path, { before, after });
      cancelFn = cancel;
      try {
        const r = await promise;
        if (!cancelled) {
          setResult(r);
        }
      } catch (e) {
        if (!cancelled && !isCancelledError(e)) {
          setErr(e instanceof Error ? e.message : String(e));
        }
      }
    };
    void attempt();
    return () => {
      cancelled = true;
      cancelFn?.();
    };
  }, [repo.repo_id, path, base, head, comparison, refreshKey]);

  const hasComments =
    commentsByLine != null && Object.keys(commentsByLine).length > 0;

  return (
    <div className="flex h-full min-h-0 flex-col bg-gx-bg-sub">
      <DiffHeader path={path} badge={result?.kind === "large_file" ? "large file" : result?.kind ?? "diff"} base={base} head={head} comparison={comparison} onComparisonChange={!base && !head ? setComparison : undefined} />
      <div className="flex min-h-0 flex-1">
        <div ref={scrollRef} className="min-h-0 flex-1 overflow-auto">
          {err && (
            <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
              {err}
            </div>
          )}
          {!err && patch == null && !largeFileDiff && (
            <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
              <Loader2 size={14} className="animate-spin" /> Running diff…
            </div>
          )}
          {!err && largeFileDiff && (
            <LargeFileDiffView data={largeFileDiff} repoId={head ? undefined : repo.repo_id} onRefresh={handleRefresh} />
          )}
          {!err && !largeFileDiff && patch != null && patch.trim().length === 0 && (
            <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
              <FileMinus size={14} /> No changes for this file.
            </div>
          )}
          {!err && !largeFileDiff && patch != null && patch.trim().length > 0 && (
            <SafePatchDiff
              patch={patch}
              className="gx-patch-diff"
              options={diffOptions(effective)}
            />
          )}
          {(hasComments || onAddComment) && (
            <CommentGutter
              commentsByLine={commentsByLine ?? {}}
              onAddComment={onAddComment}
            />
          )}
        </div>
        {minimap && !largeFileDiff && patch != null && patch.trim().length > 0 && (
          <PatchMinimap patch={patch} scrollContainer={scrollRef.current} />
        )}
      </div>
    </div>
  );
}
