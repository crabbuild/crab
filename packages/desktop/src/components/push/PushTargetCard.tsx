import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import {
  AlertTriangle,
  Check,
  ChevronDown,
  ChevronRight,
  Copy,
  GitBranch,
  GitCommit,
  GitPullRequest,
} from "lucide-react";
import { useState } from "react";
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
    <div className="rounded border border-border bg-background">
      {/* ── Branch row ───────────────────────────────────────────── */}
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2.5 text-sm">
        <div className="flex items-center gap-1.5 text-foreground">
          <GitBranch size={12} className="text-muted-foreground" />
          <span className="font-mono">
            {target.detached ? "(detached HEAD)" : (target.branch ?? "…")}
          </span>
        </div>

        <span className="text-muted-foreground">→</span>

        <div className="flex items-center gap-1.5">
          {selected ? (
            <span className="font-mono text-foreground">
              {selected.name}
              <span className="text-muted-foreground"> / </span>
              {destinationBranch || target.branch || "?"}
            </span>
          ) : target.upstream.full ? (
            <span className="font-mono text-muted-foreground">
              {target.upstream.full}
            </span>
          ) : (
            <span className="text-muted-foreground">no destination</span>
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
        <div className="flex items-center gap-2 border-t border-border/60 px-3 py-1.5 text-xs">
          <code className="flex-1 truncate font-mono text-muted-foreground" title={selected.url}>
            {selected.url}
          </code>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="text-muted-foreground transition hover:text-foreground"
            title="Copy URL"
            onClick={() => {
              void navigator.clipboard.writeText(selected.url).then(() => {
                setCopied(true);
                setTimeout(() => setCopied(false), 1500);
              });
            }}
          >
            {copied ? (
              <Check size={12} className="text-success" />
            ) : (
              <Copy size={12} />
            )}
          </Button>
        </div>
      )}

      {/* ── Warnings (inline, non-blocking) ───────────────────────── */}
      {target.upstream.behind > 0 && (
        <Banner tone="warn">
          <AlertTriangle size={12} className="shrink-0" />
          <span className="flex-1">
            Remote has {target.upstream.behind} new commit
            {target.upstream.behind === 1 ? "" : "s"}. Pulling first avoids a
            force-push.
          </span>
          {onPullFirst && (
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="text-warning underline-offset-2 hover:underline"
              onClick={onPullFirst}
            >
              Pull
            </Button>
          )}
        </Banner>
      )}
      {!target.upstream.full && !target.detached && target.branch && selected && (
          <Banner tone="info">
            <AlertTriangle size={12} className="shrink-0" />
            <span className="flex-1">
              <span className="font-mono">{target.branch}</span> has no
              upstream. The push will set{" "}
              <span className="font-mono">
                {selected.name}/{destinationBranch}
              </span>{" "}
              as the tracking branch.
            </span>
            {onSetUpstream && (
              <Button
                variant="ghost"
                size="compact"
                type="button"
                className="text-primary underline-offset-2 hover:underline"
                onClick={onSetUpstream}
              >
                Configure
              </Button>
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
              <span className="font-mono">{selected.name}</span>. Double-check
              this is intentional.
            </span>
          </Banner>
        )}

      {/* ── Change destination disclosure ────────────────────────── */}
      <Button
        variant="ghost"
        size="compact"
        type="button"
        className="flex w-full items-center gap-1 border-t border-border/60 px-3 py-1.5 text-[11px] text-muted-foreground transition hover:text-foreground"
        onClick={() => setExpanded(!expanded)}
        aria-expanded={expanded}
      >
        {expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        Change destination
      </Button>

      {expanded && (
        <div className="space-y-2 border-t border-border/60 px-3 py-2 text-xs">
          <RemotePicker
            remotes={target.crabRemotes}
            selected={selectedRemote}
            onSelect={onSelectRemote}
          />
          <div className="flex items-center gap-2">
            <label htmlFor="push-dest-branch" className="text-muted-foreground">
              Push as:
            </label>
            <span className="font-mono text-muted-foreground">
              {selectedRemote ?? "?"}/
            </span>
            <Input
              id="push-dest-branch"
              type="text"
              value={destinationBranch}
              onChange={(e) => onChangeDestinationBranch(e.target.value)}
              spellCheck={false}
              className="flex-1 rounded border border-border bg-background px-2 py-1 font-mono text-xs focus-visible:outline-none focus:border-primary"
              placeholder={target.branch ?? "branch"}
            />
          </div>
          {target.remotes.length > target.crabRemotes.length && (
            <div className="text-[11px] text-muted-foreground">
              {target.remotes.length - target.crabRemotes.length} non-crab
              remote
              {target.remotes.length - target.crabRemotes.length === 1 ? "" : "s"}{" "}
              hidden (only crab:// URLs can stream pointer files).
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
      <div className="rounded border border-warning/40 bg-warning/10 p-2 text-[11px] text-warning">
        No <code className="font-mono">crab://</code> remotes configured. Add
        one with{" "}
        <code className="font-mono">
          git remote add crab crab://&lt;bucket&gt;/&lt;repo&gt;
        </code>
        .
      </div>
    );
  }
  return (
    <RadioGroup
      aria-label="Push remote"
      value={selected ?? ""}
      onValueChange={onSelect}
      className="space-y-1"
    >
      {remotes.map((r) => {
        const active = r.name === selected;
        return (
          <label
            key={r.name}
            className={
              "flex w-full items-center gap-2 rounded border px-2 py-1.5 text-left transition " +
              (active
                ? "border-primary bg-primary/10 text-foreground"
                : "border-border hover:border-border/80 hover:bg-background")
            }
          >
            <RadioGroupItem value={r.name} />
            <span className="font-mono text-xs">{r.name}</span>
            <span className="flex-1 truncate font-mono text-[11px] text-muted-foreground">
              {r.url}
            </span>
          </label>
        );
      })}
    </RadioGroup>
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
      ? "border-primary/40 bg-primary/10 text-foreground"
      : tone === "warn"
        ? "border-warning/40 bg-warning/10 text-warning"
        : "border-border bg-background text-muted-foreground";
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
      ? "border-warning/40 bg-warning/10 text-warning"
      : tone === "error"
        ? "border-destructive/40 bg-destructive/10 text-destructive"
        : "border-primary/40 bg-primary/5 text-muted-foreground";
  return (
    <div className={`flex items-center gap-2 border-t border-x-0 px-3 py-1.5 text-[11px] ${cls}`}>
      {children}
    </div>
  );
}
