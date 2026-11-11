import { useEffect, useMemo, useState } from "react";
import clsx from "clsx";
import { ArrowRight, Clock, GitBranch, GitCommitVertical, GitMerge, Tag, User } from "lucide-react";
import { CommitRef, CommitRow } from "../ipc";

interface Props {
  commit: CommitRow;
  /** True when the commit is an ancestor of HEAD. Populated from
   *  the parent view which already computes the trunk for layout. */
  isOnHead: boolean;
  /** Anchor DOM rect — usually the commit row the user is hovering. */
  anchor: DOMRect;
  /** Distance between the anchor and the popover, in px. */
  offset?: number;
}

/**
 * Hover popover for a commit. Stays bounded to the viewport so it
 * doesn't overflow into chrome. Sits above the row by default but
 * flips below when the row is near the top of the viewport.
 */
export function CommitPopover({ commit, isOnHead, anchor, offset = 10 }: Props) {
  const [viewport, setViewport] = useState(() => ({
    w: typeof window === "undefined" ? 1280 : window.innerWidth,
    h: typeof window === "undefined" ? 820 : window.innerHeight,
  }));

  useEffect(() => {
    const onResize = () =>
      setViewport({ w: window.innerWidth, h: window.innerHeight });
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);

  const POPOVER_WIDTH = 360;
  const POPOVER_MAX_HEIGHT = 340;

  // Preferred: anchored to the anchor's right edge + offset so it
  // clears the commit row. Flip to the left if we'd overflow.
  const { top, left } = useMemo(() => {
    const x = Math.min(
      anchor.right + offset,
      viewport.w - POPOVER_WIDTH - 8,
    );
    const spaceBelow = viewport.h - anchor.top;
    const topCandidate =
      spaceBelow >= POPOVER_MAX_HEIGHT + 16
        ? anchor.top
        : Math.max(8, viewport.h - POPOVER_MAX_HEIGHT - 8);
    return { top: topCandidate, left: Math.max(8, x) };
  }, [anchor, offset, viewport]);

  const refs = commit.refs ?? [];
  const branches = refs.filter(
    (r) => r.kind === "branch" || r.kind === "head_target" || r.kind === "head",
  );
  const remotes = refs.filter((r) => r.kind === "remote");
  const tags = refs.filter((r) => r.kind === "tag");

  // Split subject / body so the first line renders bold and the
  // rest as mono-ish multi-line text. Commit messages normally
  // separate body with a blank line after the subject.
  const { subject, body } = useMemo(() => {
    const idx = commit.subject.indexOf("\n\n");
    if (idx === -1) return { subject: commit.subject, body: "" };
    return {
      subject: commit.subject.slice(0, idx),
      body: commit.subject.slice(idx + 2),
    };
  }, [commit.subject]);

  return (
    <div
      role="tooltip"
      style={{
        position: "fixed",
        top,
        left,
        width: POPOVER_WIDTH,
        maxHeight: POPOVER_MAX_HEIGHT,
        zIndex: 50,
      }}
      className="pointer-events-none flex flex-col gap-2 overflow-hidden rounded-lg border border-border bg-card p-3 text-[11.5px] text-foreground shadow-2xl"
    >
      <div className="text-[13px] font-semibold leading-tight">{subject}</div>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[10.5px] text-muted-foreground">
        <span className="flex items-center gap-1 font-mono">
          <GitCommitVertical size={11} /> {commit.sha.slice(0, 12)}
        </span>
        <span className="flex items-center gap-1">
          <User size={11} /> {commit.author}
        </span>
        <span className="flex items-center gap-1">
          <Clock size={11} /> {formatDateTime(commit.date)}
        </span>
        {commit.parents.length > 1 && (
          <span className="flex items-center gap-1 text-primary">
            <GitMerge size={11} /> merge · {commit.parents.length} parents
          </span>
        )}
      </div>

      {body && (
        <div className="overflow-auto whitespace-pre-wrap rounded border border-border/60 bg-background p-2 font-mono text-[11px] text-foreground/90">
          {body}
        </div>
      )}

      <div className="flex items-center gap-1 text-[10.5px]">
        {isOnHead ? (
          <span className="flex items-center gap-1 text-success">
            <ArrowRight size={11} /> contained in HEAD
          </span>
        ) : (
          <span className="text-muted-foreground">not an ancestor of HEAD</span>
        )}
      </div>

      {(branches.length > 0 || remotes.length > 0 || tags.length > 0) && (
        <div className="space-y-1">
          {branches.length > 0 && (
            <RefSection
              icon={<GitBranch size={10} />}
              label="Branches"
              refs={branches}
            />
          )}
          {remotes.length > 0 && (
            <RefSection
              icon={<GitBranch size={10} />}
              label="Remotes"
              refs={remotes}
            />
          )}
          {tags.length > 0 && (
            <RefSection icon={<Tag size={10} />} label="Tags" refs={tags} />
          )}
        </div>
      )}

      {commit.parents.length > 0 && (
        <div className="flex flex-wrap items-center gap-1 text-[10px] text-muted-foreground">
          <span className="uppercase tracking-wider">Parents:</span>
          {commit.parents.map((p) => (
            <code
              key={p}
              className="rounded bg-background px-1 py-[1px] font-mono text-[10px] text-muted-foreground"
            >
              {p.slice(0, 7)}
            </code>
          ))}
        </div>
      )}
    </div>
  );
}

function RefSection({
  icon,
  label,
  refs,
}: {
  icon: React.ReactNode;
  label: string;
  refs: CommitRef[];
}) {
  return (
    <div className="flex items-start gap-1.5 text-[10px]">
      <span className="mt-[2px] text-muted-foreground">{icon}</span>
      <span className="mt-[2px] w-14 shrink-0 uppercase tracking-wider text-muted-foreground">
        {label}
      </span>
      <div className="flex min-w-0 flex-wrap gap-1">
        {refs.map((r, i) => {
          const short = r.name
            .replace(/^refs\/heads\//, "")
            .replace(/^refs\/tags\//, "")
            .replace(/^refs\/remotes\//, "");
          const cls = clsx(
            "max-w-[160px] truncate rounded-full border bg-background px-1.5 py-[1px] font-mono text-[9px]",
            r.kind === "tag"
              ? "border-warning/50 text-warning"
              : r.kind === "head" || r.kind === "head_target"
                ? "border-primary/50 text-primary"
                : r.kind === "remote"
                  ? "border-muted-foreground/40 text-muted-foreground"
                  : "border-success/40 text-success",
          );
          return (
            <span key={`${r.name}-${i}`} className={cls} title={r.name}>
              {short}
            </span>
          );
        })}
      </div>
    </div>
  );
}

function formatDateTime(iso: string): string {
  try {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    return d.toLocaleString();
  } catch {
    return iso;
  }
}
