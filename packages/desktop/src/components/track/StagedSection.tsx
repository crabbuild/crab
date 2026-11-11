import { useState } from "react";
import {
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  FileCog,
  GitCommit,
  HardDrive,
  Loader2,
  Upload,
  XCircle,
} from "lucide-react";
import { StagedFileEntry, StagingStats } from "../../ipc";
import { formatBytes } from "../../lib/format";

/** Per-file status the section needs to render the right CTA. */
export type StagedStatus =
  | "uncommitted" // Crab-staged + git working-tree (modified or untracked from git's view)
  | "git-staged" // Crab-staged + `git add` ran but no commit yet
  | "committed"; // Crab-staged + committed locally; just needs `git push`

interface Props {
  stats: StagingStats | null;
  loading: boolean;
  /** git porcelain status entries (path → 2-char code), used to derive per-file commit state. */
  gitStatusByPath: Map<string, string>;
  /** True if the working tree has any uncommitted changes touching staged paths. */
  needsCommit: boolean;
  /** True if we have at least one local commit ahead of upstream. */
  hasUnpushedCommits: boolean;
  /** True while the user is removing a specific file from staging. */
  removingFiles: Set<string>;
  onRemoveFile: (fileHash: string) => void;
  onShowPush: () => void;
  /** Inline commit affordance — the commit message comes from the user. */
  onCommit: (message: string) => Promise<void>;
}

/**
 * Renders the "Staged in Crab" section of the Track & Stage tab.
 *
 * Surfaces files that have already been chunked into `.crab/staging`,
 * their commit status (working-tree dirty / git-staged / committed),
 * and a context-aware action — commit them inline if the working tree
 * is dirty, otherwise push.
 *
 * The section is intentionally always rendered (even when empty)
 * after the first fetch so a `crab add` from the CLI has a stable
 * place to land in the desktop UI without the panel jumping around.
 */
export function StagedSection({
  stats,
  loading,
  gitStatusByPath,
  needsCommit,
  hasUnpushedCommits,
  removingFiles,
  onRemoveFile,
  onShowPush,
  onCommit,
}: Props) {
  const [expanded, setExpanded] = useState(true);
  const [committing, setCommitting] = useState(false);
  const [commitMessage, setCommitMessage] = useState("");
  const [commitError, setCommitError] = useState<string | null>(null);

  // A staged recipe can reuse every payload chunk already in storage.
  // Zero local chunks still requires committing its changed Git pointer.
  const files = stats?.files ?? [];

  // Don't render at all when the agent hasn't responded yet AND we have
  // no cached files — avoids a flash of "empty" before the first fetch.
  if (!stats && loading) {
    return (
      <div className="border-b border-gx-border px-4 py-2.5 text-[11px] text-gx-muted">
        <Loader2 size={11} className="mr-1 inline animate-spin" />
        Checking staging area…
      </div>
    );
  }
  if (!stats) return null;
  if (files.length === 0 && !needsCommit && !hasUnpushedCommits) return null;

  const totalBytes = stats.total_staged_bytes ?? 0;
  const totalChunks = stats.chunk_count ?? 0;

  async function handleCommit() {
    if (!commitMessage.trim() || committing) return;
    setCommitting(true);
    setCommitError(null);
    try {
      await onCommit(commitMessage.trim());
      setCommitMessage("");
    } catch (e) {
      setCommitError(e instanceof Error ? e.message : String(e));
    } finally {
      setCommitting(false);
    }
  }

  return (
    <div className="border-b border-gx-border">
      <button
        type="button"
        onClick={() => setExpanded(!expanded)}
        className="flex w-full items-center gap-2 px-4 py-2 text-left text-[11px] font-semibold uppercase tracking-wider text-gx-muted hover:bg-gx-surface-hover"
      >
        {expanded ? <ChevronDown size={10} /> : <ChevronRight size={10} />}
        <HardDrive size={11} className="text-gx-accent" />
        Staged in Crab ({files.length})
        {totalBytes > 0 && (
          <span className="ml-1 text-[10px] normal-case tracking-normal text-gx-muted">
            · {formatBytes(totalBytes)}
            {totalChunks > 0 && ` · ${totalChunks.toLocaleString()} chunks`}
          </span>
        )}
      </button>

      {expanded && (
        <>
          {/* Per-file rows */}
          {files.length > 0 && (
            <div className="ml-6 border-l border-gx-border/30">
              {files.map((f) => (
                <StagedFileRow
                  key={f.file_hash}
                  file={f}
                  status={statusFor(f, gitStatusByPath)}
                  removing={removingFiles.has(f.file_hash)}
                  onRemove={() => onRemoveFile(f.file_hash)}
                />
              ))}
            </div>
          )}

          {/* Action panel */}
          <div className="space-y-2 px-4 py-3">
            {needsCommit ? (
              <CommitPrompt
                message={commitMessage}
                onChange={setCommitMessage}
                committing={committing}
                error={commitError}
                onCommit={handleCommit}
              />
            ) : (
              <PushPrompt
                hasUnpushedCommits={hasUnpushedCommits}
                onShowPush={onShowPush}
              />
            )}
            <p className="text-[10px] text-gx-muted">
              Staged chunks live in <code className="font-mono">.crab/staging/</code>{" "}
              until pushed. Already-pushed files appear in Storage Map.
            </p>
          </div>
        </>
      )}
    </div>
  );
}

// ── Subcomponents ───────────────────────────────────────────────────

function CommitPrompt({
  message,
  onChange,
  committing,
  error,
  onCommit,
}: {
  message: string;
  onChange: (v: string) => void;
  committing: boolean;
  error: string | null;
  onCommit: () => void;
}) {
  return (
    <div className="space-y-2 rounded border border-gx-warn/40 bg-gx-warn/10 p-2.5 text-xs">
      <div className="flex items-center gap-2 font-medium text-gx-warn">
        <GitCommit size={12} />
        <span>Crab-tracked changes are not committed yet</span>
      </div>
      <p className="text-[11px] text-gx-muted">
        Commit them so the push can include the new pointer blobs.
      </p>
      <div className="flex gap-2">
        <input
          type="text"
          value={message}
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              onCommit();
            }
          }}
          placeholder="Commit message"
          className="flex-1 rounded border border-gx-border bg-gx-bg px-2 py-1 text-xs focus-visible:outline-none focus:border-gx-accent"
          disabled={committing}
        />
        <button
          type="button"
          className="btn btn-primary text-xs"
          onClick={onCommit}
          disabled={!message.trim() || committing}
        >
          {committing ? (
            <>
              <Loader2 size={11} className="animate-spin" /> Committing…
            </>
          ) : (
            <>
              <GitCommit size={11} /> Commit
            </>
          )}
        </button>
      </div>
      {error && (
        <div className="text-[11px] text-gx-error">
          <XCircle size={10} className="mr-1 inline" />
          {error}
        </div>
      )}
    </div>
  );
}

function PushPrompt({
  hasUnpushedCommits,
  onShowPush,
}: {
  hasUnpushedCommits: boolean;
  onShowPush: () => void;
}) {
  return (
    <div className="flex items-center gap-2">
      <button type="button" className="btn btn-primary text-xs" onClick={onShowPush}>
        <Upload size={11} /> Push to Remote
      </button>
      {hasUnpushedCommits && (
        <span className="text-[11px] text-gx-muted">
          <CheckCircle2 size={10} className="mr-1 inline text-gx-success" />
          Commits are local — push to share.
        </span>
      )}
    </div>
  );
}

function StagedFileRow({
  file,
  status,
  removing,
  onRemove,
}: {
  file: StagedFileEntry;
  status: StagedStatus;
  removing: boolean;
  onRemove: () => void;
}) {
  // Prefer the recorded path; fall back to a short hash if path
  // metadata wasn't captured (older crab versions).
  const label = file.file_path ?? `${file.file_hash.slice(0, 12)}…`;
  return (
    <div className="group flex items-center gap-2 px-4 py-1.5 text-xs hover:bg-gx-surface-hover">
      <FileCog size={13} className="shrink-0 text-orange-400" />
      <span
        className="min-w-0 flex-1 truncate font-mono text-gx-text"
        title={file.file_path ? `${file.file_path}\n${file.file_hash}` : file.file_hash}
      >
        {label}
      </span>
      <StatusBadge status={status} />
      <span className="shrink-0 tabular-nums text-gx-muted">{formatBytes(file.total_bytes)}</span>
      <span className="shrink-0 rounded bg-gx-accent/10 px-1.5 py-0.5 text-[10px] text-gx-accent">
        {file.total_chunks} chunks
      </span>
      {file.pending_chunks > 0 && (
        <span className="shrink-0 rounded bg-gx-warn/10 px-1.5 py-0.5 text-[10px] text-gx-warn">
          {file.pending_chunks} pending
        </span>
      )}
      <button
        type="button"
        className="shrink-0 rounded p-0.5 text-gx-muted transition hover:bg-gx-error/10 hover:text-gx-error"
        onClick={onRemove}
        disabled={removing}
        title="Remove from staging area"
      >
        {removing ? <Loader2 size={11} className="animate-spin" /> : <XCircle size={11} />}
      </button>
    </div>
  );
}

function StatusBadge({ status }: { status: StagedStatus }) {
  const cfg = {
    uncommitted: { label: "uncommitted", cls: "border-gx-warn/40 bg-gx-warn/10 text-gx-warn" },
    "git-staged": { label: "to commit", cls: "border-gx-accent/40 bg-gx-accent/10 text-gx-accent" },
    committed: { label: "committed", cls: "border-gx-success/40 bg-gx-success/10 text-gx-success" },
  }[status];
  return (
    <span
      className={`shrink-0 rounded border px-1 text-[9px] uppercase tracking-wider ${cfg.cls}`}
    >
      {cfg.label}
    </span>
  );
}

// ── Status derivation ──────────────────────────────────────────────

function statusFor(
  file: StagedFileEntry,
  gitStatusByPath: Map<string, string>,
): StagedStatus {
  const path = file.file_path;
  if (!path) return "committed";
  const code = gitStatusByPath.get(path);
  if (!code) return "committed";
  // git porcelain XY codes:
  //   X = index status, Y = working tree
  //   "??" = untracked, "A " = added, " M" = modified worktree, "M " = staged mod
  if (code === "??") return "uncommitted";
  // Index column non-blank, non-`?` means staged. Check that first
  // because "MM" (staged & dirty) should still surface as git-staged.
  const indexCol = code[0] ?? " ";
  if (indexCol !== " " && indexCol !== "?") return "git-staged";
  // Worktree-only changes → uncommitted from the user's perspective.
  return "uncommitted";
}
