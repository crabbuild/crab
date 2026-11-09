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

/** Compare selected versions through one cancellable capture operation. */
export function DiffView({ repo, path, base, head, commentsByLine, onAddComment }: Props) {
  const [result, setResult] = useState<FileComparison | null>(null);
  const patch = result?.kind === "git" ? result.patch : null;
  const largeFileDiff = result?.kind === "large_file" ? result : null;
  const notebook = result?.kind === "notebook" ? result : null;
  const [err, setErr] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);
  const [stopped, setStopped] = useState(false);
  const cancelRef = useRef<(() => void) | null>(null);
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
    setStopped(false);
    cancelRef.current = () => { cancelled = true; cancelFn?.(); setStopped(true); };

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
      cancelRef.current = null;
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
            <div role="alert" className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
              {err}
              <button className="btn ml-2" onClick={handleRefresh}>Retry comparison</button>
            </div>
          )}
          {!err && result == null && (
            <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
              {stopped ? <>Comparison cancelled. <button className="btn" onClick={handleRefresh}>Retry comparison</button></>
                : <><Loader2 size={14} className="animate-spin" /> Running diff… <button className="btn" onClick={() => cancelRef.current?.()}>Cancel comparison</button></>}
            </div>
          )}
          {!err && largeFileDiff && (
            <LargeFileDiffView data={largeFileDiff} repoId={head ? undefined : repo.repo_id} onRefresh={handleRefresh} />
          )}
          {!err && notebook && <NotebookDiffView data={notebook} onRefresh={handleRefresh} />}
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
          {result?.kind === "git" && (hasComments || onAddComment) && (
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
