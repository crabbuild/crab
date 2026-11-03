import { useState } from "react";
import {
  AlertTriangle,
  ChevronDown,
  ChevronRight,
  Copy,
  Check,
  GitBranch,
  GitCommit,
  GitPullRequest,
} from "lucide-react";
import { PushRemote, PushTargetInfo } from "../../hooks/usePushTarget";

interface Props {
  target: PushTargetInfo;
  /** Currently selected destination remote name, or null when nothing is selected/available. */
  selectedRemote: string | null;
  onSelectRemote: (remoteName: string) => void;
  /** Push-as branch name on the remote (defaults to upstream's branch or local branch name). */
  destinationBranch: string;
  onChangeDestinationBranch: (branch: string) => void;
  /** Callback for the inline "Pull first" affordance shown when behind upstream. */
  onPullFirst?: () => void;
  /** Triggered when the user opts to set the upstream tracking ref. */
  onSetUpstream?: () => void;
}

/**
 * Always-visible card showing where a push will go: the source branch,
 * destination remote (name + crab:// URL), and ahead/behind counts.
 *
 * The "Change destination" disclosure stays collapsed by default so the
 * common case (a single crab remote, upstream set) reads as a single
 * line of confident text. Edge cases — no upstream, behind upstream,
 * mixed-remote URL mismatch — surface as inline warnings rather than
 * blocking dialogs.
 */
export function PushTargetCard({
  target,
  selectedRemote,
  onSelectRemote,
  destinationBranch,
  onChangeDestinationBranch,
  onPullFirst,
  onSetUpstream,
}: Props) {
  const [expanded, setExpanded] = useState(false);
  const [copied, setCopied] = useState(false);

  const selected = target.crabRemotes.find((r) => r.name === selectedRemote) ?? null;
  const upstreamRemoteMatches =
    target.upstream.remote === selectedRemote ||
    target.upstream.remote === null; // No upstream → no mismatch to flag

  // Empty / detached / no-remote states are handled by ad-hoc panels in
  // the parent, but we still render the card chrome so layout doesn't
  // jump between states.

  return (
    <div className="rounded border border-gx-border bg-gx-bg-sub">
      {/* ── Branch row ───────────────────────────────────────────── */}
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2.5 text-sm">
        <div className="flex items-center gap-1.5 text-gx-text">
          <GitBranch size={12} className="text-gx-muted" />
          <span className="font-mono">
            {target.detached ? "(detached HEAD)" : (target.branch ?? "…")}
          </span>
        </div>

        <span className="text-gx-muted">→</span>

        <div className="flex items-center gap-1.5">
          {selected ? (
            <span className="font-mono text-gx-text">
              {selected.name}
              <span className="text-gx-muted"> / </span>
              {destinationBranch || target.branch || "?"}
            </span>
          ) : target.upstream.full ? (
            <span className="font-mono text-gx-muted">{target.upstream.full}</span>
          ) : (
            <span className="text-gx-muted">no destination</span>
          )}
        </div>

        {/* Ahead/behind chips, when an upstream is set */}
        {target.upstream.full && (
          <div className="flex items-center gap-1 text-[11px]">
            <Chip
              icon={<GitCommit size={10} />}
              label={`↑${target.upstream.ahead}`}
              tone={target.upstream.ahead > 0 ? "info" : "muted"}
              title={`${target.upstream.ahead} local commit(s) not on upstream`}
            />
            <Chip
              icon={<GitPullRequest size={10} />}
              label={`↓${target.upstream.behind}`}
              tone={target.upstream.behind > 0 ? "warn" : "muted"}
              title={`${target.upstream.behind} upstream commit(s) not local`}
            />
          </div>
        )}
      </div>

      {/* ── URL row ──────────────────────────────────────────────── */}
      {selected && (
        <div className="flex items-center gap-2 border-t border-gx-border/60 px-3 py-1.5 text-xs">
          <code className="flex-1 truncate font-mono text-gx-muted" title={selected.url}>
            {selected.url}
          </code>
          <button
            type="button"
            className="text-gx-muted transition hover:text-gx-text"
            title="Copy URL"
            onClick={() => {
              void navigator.clipboard.writeText(selected.url).then(() => {
                setCopied(true);
                setTimeout(() => setCopied(false), 1500);
              });
            }}
          >
            {copied ? <Check size={12} className="text-gx-success" /> : <Copy size={12} />}
          </button>
        </div>
      )}

      {/* ── Warnings (inline, non-blocking) ───────────────────────── */}
      {target.upstream.behind > 0 && (
        <Banner tone="warn">
          <AlertTriangle size={12} className="shrink-0" />
          <span className="flex-1">
            Remote has {target.upstream.behind} new commit
            {target.upstream.behind === 1 ? "" : "s"}. Pulling first avoids a force-push.
          </span>
          {onPullFirst && (
            <button
              type="button"
              className="text-gx-warn underline-offset-2 hover:underline"
              onClick={onPullFirst}
            >
              Pull
            </button>
          )}
        </Banner>
      )}
      {!target.upstream.full && !target.detached && target.branch && selected && (
        <Banner tone="info">
          <AlertTriangle size={12} className="shrink-0" />
          <span className="flex-1">
            <span className="font-mono">{target.branch}</span> has no upstream. The push will
            set <span className="font-mono">{selected.name}/{destinationBranch}</span> as
            the tracking branch.
          </span>
          {onSetUpstream && (
            <button
              type="button"
              className="text-gx-accent underline-offset-2 hover:underline"
              onClick={onSetUpstream}
            >
              Configure
            </button>
          )}
        </Banner>
      )}
      {target.upstream.full &&
        selected &&
        target.upstream.remote !== null &&
        !upstreamRemoteMatches && (
          <Banner tone="warn">
            <AlertTriangle size={12} className="shrink-0" />
            <span className="flex-1">
              Upstream is{" "}
              <span className="font-mono">
                {target.upstream.remote}/{target.upstream.branch}
              </span>{" "}
              but you're pushing to{" "}
              <span className="font-mono">{selected.name}</span>. Double-check this is
              intentional.
            </span>
          </Banner>
        )}

      {/* ── Change destination disclosure ────────────────────────── */}
      <button
        type="button"
        className="flex w-full items-center gap-1 border-t border-gx-border/60 px-3 py-1.5 text-[11px] text-gx-muted transition hover:text-gx-text"
        onClick={() => setExpanded(!expanded)}
        aria-expanded={expanded}
      >
        {expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        Change destination
      </button>

      {expanded && (
        <div className="space-y-2 border-t border-gx-border/60 px-3 py-2 text-xs">
          <RemotePicker
            remotes={target.crabRemotes}
            selected={selectedRemote}
            onSelect={onSelectRemote}
          />
          <div className="flex items-center gap-2">
            <label htmlFor="push-dest-branch" className="text-gx-muted">
              Push as:
            </label>
            <span className="font-mono text-gx-muted">{selectedRemote ?? "?"}/</span>
            <input
              id="push-dest-branch"
              type="text"
              value={destinationBranch}
              onChange={(e) => onChangeDestinationBranch(e.target.value)}
              spellCheck={false}
              className="flex-1 rounded border border-gx-border bg-gx-bg px-2 py-1 font-mono text-xs focus-visible:outline-none focus:border-gx-accent"
              placeholder={target.branch ?? "branch"}
            />
          </div>
          {target.remotes.length > target.crabRemotes.length && (
            <div className="text-[11px] text-gx-muted">
              {target.remotes.length - target.crabRemotes.length} non-crab remote
              {target.remotes.length - target.crabRemotes.length === 1 ? "" : "s"} hidden
              (only crab:// URLs can stream pointer files).
            </div>
          )}
        </div>
      )}
    </div>
  );
}

// ── Subcomponents ───────────────────────────────────────────────────

function RemotePicker({
  remotes,
  selected,
  onSelect,
}: {
  remotes: PushRemote[];
  selected: string | null;
  onSelect: (name: string) => void;
}) {
  if (remotes.length === 0) {
    return (
      <div className="rounded border border-gx-warn/40 bg-gx-warn/10 p-2 text-[11px] text-gx-warn">
        No <code className="font-mono">crab://</code> remotes configured. Add one with{" "}
        <code className="font-mono">git remote add crab crab://&lt;bucket&gt;/&lt;repo&gt;</code>.
      </div>
    );
  }
  return (
    <div className="space-y-1">
      {remotes.map((r) => {
        const active = r.name === selected;
        return (
          <button
            key={r.name}
            type="button"
            onClick={() => onSelect(r.name)}
            className={
              "flex w-full items-center gap-2 rounded border px-2 py-1.5 text-left transition " +
              (active
                ? "border-gx-accent bg-gx-accent/10 text-gx-text"
                : "border-gx-border hover:border-gx-border/80 hover:bg-gx-bg")
            }
          >
            <input
              type="radio"
              className="accent-gx-accent"
              checked={active}
              readOnly
            />
            <span className="font-mono text-xs">{r.name}</span>
            <span className="flex-1 truncate font-mono text-[11px] text-gx-muted">
              {r.url}
            </span>
          </button>
        );
      })}
    </div>
  );
}

function Chip({
  icon,
  label,
  tone,
  title,
}: {
  icon: React.ReactNode;
  label: string;
  tone: "info" | "warn" | "muted";
  title?: string;
}) {
  const cls =
    tone === "info"
      ? "border-gx-accent/40 bg-gx-accent/10 text-gx-text"
      : tone === "warn"
        ? "border-gx-warn/40 bg-gx-warn/10 text-gx-warn"
        : "border-gx-border bg-gx-bg text-gx-muted";
  return (
    <span
      className={`inline-flex items-center gap-1 rounded border px-1.5 py-0.5 font-mono ${cls}`}
      title={title}
    >
      {icon}
      {label}
    </span>
  );
}

function Banner({
  tone,
  children,
}: {
  tone: "info" | "warn" | "error";
  children: React.ReactNode;
}) {
  const cls =
    tone === "warn"
      ? "border-gx-warn/40 bg-gx-warn/10 text-gx-warn"
      : tone === "error"
        ? "border-gx-error/40 bg-gx-error/10 text-gx-error"
        : "border-gx-accent/40 bg-gx-accent/5 text-gx-muted";
  return (
    <div className={`flex items-center gap-2 border-t border-x-0 px-3 py-1.5 text-[11px] ${cls}`}>
      {children}
    </div>
  );
}
