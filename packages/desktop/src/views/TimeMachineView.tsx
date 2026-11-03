import { memo, useCallback, useEffect, useMemo, useState } from "react";
import { Clock, Loader2, User } from "lucide-react";
import { OpenRepo } from "../state";
import { CommitRow, rpc } from "../ipc";

// ── Types ────────────────────────────────────────────────────────────

export interface TimeMachineViewProps {
  repo: OpenRepo;
  path: string;
}

interface BlameLine {
  sha: string;
  author: string;
  line: number;
  content: string;
}

// ── Helpers ──────────────────────────────────────────────────────────

/** Deterministic colour from an author string for blame gutters. */
function authorColor(author: string): string {
  let hash = 0;
  for (let i = 0; i < author.length; i++) {
    hash = author.charCodeAt(i) + ((hash << 5) - hash);
  }
  const hue = Math.abs(hash) % 360;
  return `hsl(${hue}, 55%, 45%)`;
}

function formatDate(iso: string): string {
  try {
    return new Date(iso).toLocaleDateString(undefined, {
      year: "numeric",
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    });
  } catch {
    return iso;
  }
}

// ── Component ────────────────────────────────────────────────────────

export const TimeMachineView = memo(function TimeMachineView({
  repo,
  path,
}: TimeMachineViewProps) {
  const [commits, setCommits] = useState<CommitRow[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  // Slider index — 0 = oldest, length-1 = newest
  const [sliderIndex, setSliderIndex] = useState<number>(0);

  // File content at the selected revision
  const [content, setContent] = useState<string | null>(null);
  const [contentLoading, setContentLoading] = useState(false);

  // Blame data at the selected revision
  const [blameLines, setBlameLines] = useState<BlameLine[] | null>(null);
  const [blameLoading, setBlameLoading] = useState(false);

  // Load commit history for the file on mount
  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);

    rpc
      .gitLog(repo.repo_id, { path, max: 200 })
      .then((res) => {
        if (cancelled) return;
        // git log returns newest-first; reverse so slider left = oldest
        const reversed = [...res.commits].reverse();
        setCommits(reversed);
        // Start at the newest commit
        setSliderIndex(reversed.length - 1);
      })
      .catch((e) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, [repo.repo_id, path]);

  // Current commit based on slider position
  const currentCommit = useMemo(
    () => (commits && commits.length > 0 ? commits[sliderIndex] : null),
    [commits, sliderIndex],
  );

  // Fetch file content + blame when the selected commit changes
  useEffect(() => {
    if (!currentCommit) return;
    let cancelled = false;

    setContentLoading(true);
    rpc
      .readText(repo.repo_id, path, { rev: currentCommit.sha })
      .then((res) => {
        if (!cancelled) setContent(res.text ?? "");
      })
      .catch(() => {
        if (!cancelled) setContent(null);
      })
      .finally(() => {
        if (!cancelled) setContentLoading(false);
      });

    setBlameLoading(true);
    rpc
      .gitBlame(repo.repo_id, path, { rev: currentCommit.sha })
      .then((res) => {
        if (!cancelled) setBlameLines(res.lines);
      })
      .catch(() => {
        if (!cancelled) setBlameLines(null);
      })
      .finally(() => {
        if (!cancelled) setBlameLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, [repo.repo_id, path, currentCommit]);

  // Unique authors for the legend
  const authors = useMemo(() => {
    if (!blameLines) return [];
    const seen = new Set<string>();
    return blameLines.reduce<string[]>((acc, l) => {
      if (!seen.has(l.author)) {
        seen.add(l.author);
        acc.push(l.author);
      }
      return acc;
    }, []);
  }, [blameLines]);

  const handleSliderChange = useCallback(
    (e: React.ChangeEvent<HTMLInputElement>) => {
      setSliderIndex(Number(e.target.value));
    },
    [],
  );

  // ── Render ───────────────────────────────────────────────────────

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center text-gx-muted">
        <Loader2 size={16} className="mr-2 animate-spin" />
        Loading file history…
      </div>
    );
  }

  if (error) {
    return (
      <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-4 text-sm text-gx-error">
        {error}
      </div>
    );
  }

  if (!commits || commits.length === 0) {
    return (
      <div className="flex h-full items-center justify-center text-gx-muted text-sm">
        No history found for <code className="mx-1 font-mono">{path}</code>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col overflow-hidden">
      {/* Header: file path + commit info */}
      <div className="shrink-0 border-b border-gx-border px-4 py-3">
        <div className="flex items-center gap-2 text-xs text-gx-muted">
          <Clock size={12} />
          <span className="font-mono">{path}</span>
          <span className="ml-auto text-[10px]">
            {sliderIndex + 1} / {commits.length} commits
          </span>
        </div>

        {currentCommit && (
          <div className="mt-2 flex flex-wrap items-center gap-3 text-xs">
            <span className="flex items-center gap-1 text-gx-text">
              <User size={11} />
              {currentCommit.author}
            </span>
            <span className="text-gx-muted">{formatDate(currentCommit.date)}</span>
            <span className="truncate text-gx-text font-medium">
              {currentCommit.subject}
            </span>
            <code className="ml-auto shrink-0 rounded bg-gx-bg-sub px-1.5 py-0.5 font-mono text-[10px] text-gx-muted">
              {currentCommit.sha.slice(0, 8)}
            </code>
          </div>
        )}

        {/* Slider */}
        <div className="mt-3">
          <input
            type="range"
            min={0}
            max={commits.length - 1}
            value={sliderIndex}
            onChange={handleSliderChange}
            className="w-full accent-gx-accent"
            aria-label="Navigate file history"
          />
          <div className="mt-1 flex justify-between text-[10px] text-gx-muted">
            <span>Oldest</span>
            <span>Newest</span>
          </div>
        </div>
      </div>

      {/* Blame legend */}
      {authors.length > 0 && (
        <div className="shrink-0 border-b border-gx-border px-4 py-1.5 flex flex-wrap gap-3">
          {authors.map((a) => (
            <span key={a} className="flex items-center gap-1 text-[10px]">
              <span
                className="inline-block h-2.5 w-2.5 rounded-sm"
                style={{ backgroundColor: authorColor(a) }}
              />
              <span className="text-gx-muted">{a}</span>
            </span>
          ))}
        </div>
      )}

      {/* File content with blame gutter */}
      <div className="min-h-0 flex-1 overflow-auto">
        {contentLoading || blameLoading ? (
          <div className="flex items-center gap-2 p-4 text-xs text-gx-muted">
            <Loader2 size={12} className="animate-spin" />
            Loading revision…
          </div>
        ) : content !== null ? (
          <div className="font-mono text-xs leading-5">
            {content.split("\n").map((line, idx) => {
              const blame = blameLines?.[idx];
              const bgColor = blame
                ? `${authorColor(blame.author)}12`
                : undefined;
              return (
                <div
                  key={idx}
                  className="flex hover:bg-gx-surface-hover/50"
                  style={bgColor ? { backgroundColor: bgColor } : undefined}
                >
                  {/* Blame gutter */}
                  <div
                    className="shrink-0 w-[180px] truncate border-r border-gx-border px-2 py-px text-[10px] text-gx-muted select-none"
                    title={blame ? `${blame.author} (${blame.sha.slice(0, 7)})` : undefined}
                  >
                    {blame && (
                      <span className="flex items-center gap-1">
                        <span
                          className="inline-block h-2 w-2 rounded-sm shrink-0"
                          style={{ backgroundColor: authorColor(blame.author) }}
                        />
                        <span className="truncate">{blame.author}</span>
                      </span>
                    )}
                  </div>
                  {/* Line number */}
                  <div className="shrink-0 w-10 text-right pr-2 py-px text-gx-muted select-none">
                    {idx + 1}
                  </div>
                  {/* Content */}
                  <pre className="flex-1 whitespace-pre px-2 py-px overflow-x-auto">
                    {line}
                  </pre>
                </div>
              );
            })}
          </div>
        ) : (
          <div className="p-4 text-xs text-gx-muted">
            Unable to load file content at this revision.
          </div>
        )}
      </div>
    </div>
  );
});
