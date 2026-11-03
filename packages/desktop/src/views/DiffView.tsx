import { useEffect, useRef, useState, useCallback } from "react";
import { SafePatchDiff } from "../components/SafePatchDiff";
import { CrabDiffView } from "../components/DiffView";
import { FileMinus, GitCompareArrows, Loader2 } from "lucide-react";
import { OpenRepo } from "../state";
import { rpc, gitDiffFileCancellable, isCancelledError } from "../ipc";
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
}: {
  path: string;
  badge: string;
  base?: string | null;
  head?: string | null;
}) {
  return (
    <div className="flex shrink-0 items-center gap-3 border-b border-gx-border bg-gx-surface px-3 py-1.5 text-[11px] text-gx-muted">
      <GitCompareArrows size={13} className="text-gx-accent" />
      <span className="font-mono text-gx-text">{path}</span>
      <span className="chip text-[9px]">{badge}</span>
      <span className="ml-auto">
        {base || head ? (
          <>
            {base ?? "HEAD"} … {head ?? "(working tree)"}
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
  const [patch, setPatch] = useState<string | null>(null);
  const [crabDiff, setCrabDiff] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [backend, setBackend] = useState<"crab" | "git">("git");
  const [refreshKey, setRefreshKey] = useState(0);
  const minimap = useSetting("editorMinimap");
  const effective = useEffectiveTheme();
  const scrollRef = useRef<HTMLDivElement>(null);

  const handleRefresh = useCallback(() => {
    setRefreshKey((k) => k + 1);
  }, []);

  useEffect(() => {
    let cancelFn: (() => void) | null = null;
    let cancelled = false;
    setPatch(null);
    setCrabDiff(null);
    setErr(null);

    const attempt = async () => {
      // Always try crab diff first — it handles large/pointer
      // files with chunk-level comparison and falls back internally
      // for git-native files. Any failure (CLI missing, non-pointer
      // file, user has no crab config) falls through to plain
      // `git diff` so the diff always renders something useful.
      try {
        const r = await rpc.crabDiffFile(repo.repo_id, path, {
          base: base ?? undefined,
          head: head ?? undefined,
        });
        if (!cancelled) {
          setPatch(r.patch || null);
          setCrabDiff(r.crab_diff ?? null);
          setBackend("crab");
        }
        return;
      } catch {
        // fall through
      }
      // Use cancellable git diff.
      const { promise, cancel } = gitDiffFileCancellable(repo.repo_id, path, {
        base: base ?? undefined,
        head: head ?? undefined,
      });
      cancelFn = cancel;
      try {
        const r = await promise;
        if (!cancelled) {
          setPatch(r.patch);
          setBackend("git");
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
  }, [repo.repo_id, path, base, head, refreshKey]);

  const hasComments =
    commentsByLine != null && Object.keys(commentsByLine).length > 0;

  return (
    <div className="flex h-full min-h-0 flex-col bg-gx-bg-sub">
      <DiffHeader path={path} badge={backend} base={base} head={head} />
      <div className="flex min-h-0 flex-1">
        <div ref={scrollRef} className="min-h-0 flex-1 overflow-auto">
          {err && (
            <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
              {err}
            </div>
          )}
          {!err && patch == null && !crabDiff && (
            <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
              <Loader2 size={14} className="animate-spin" /> Running diff…
            </div>
          )}
          {!err && crabDiff && (
            <CrabDiffView data={crabDiff} path={path} repoId={repo.repo_id} onRefresh={handleRefresh} />
          )}
          {!err && !crabDiff && patch != null && patch.trim().length === 0 && (
            <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
              <FileMinus size={14} /> No changes for this file.
            </div>
          )}
          {!err && !crabDiff && patch != null && patch.trim().length > 0 && (
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
        {minimap && !crabDiff && patch != null && patch.trim().length > 0 && (
          <PatchMinimap patch={patch} scrollContainer={scrollRef.current} />
        )}
      </div>
    </div>
  );
}
