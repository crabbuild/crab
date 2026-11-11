import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import {
  CheckCircle2,
  ChevronDown,
  Clock,
  Eye,
  FileCode2,
  GitCommitHorizontal,
  GitMerge,
  GitPullRequest,
  Loader2,
  MessageSquare,
  RefreshCw,
  Send,
  ShieldAlert,
  Tag,
  User,
  Users,
  X,
  XCircle,
} from "lucide-react";
import { Streamdown } from "streamdown";
import "streamdown/styles.css";
import { OpenRepo } from "../state";
import {
  PrChangedFile,
  PrInlineComment,
  PrTimelineEvent,
  PullRequestDetail,
  PullRequestStatus,
  ReviewVerdict,
  rpc,
} from "../ipc";
import { DiffView, type CommentsByLine } from "./DiffView";
import { aggregateCheckStatus } from "../components/CiStatusChip";
import { SafePatchDiff } from "../components/SafePatchDiff";
import { useEffectiveTheme } from "../hooks/useEffectiveTheme";
import { diffOptions } from "../lib/diffTheme";
import type { DiffLineAnnotation } from "@pierre/diffs/react";
import type { SelectedLineRange } from "@pierre/diffs";
import { PendingReviewProvider, usePendingReview } from "../review/PendingReviewContext";
import { FileProgressProvider, useFileProgress } from "../review/FileProgressContext";
import { PendingReviewFab } from "../components/review/PendingReviewFab";
import { ReviewSubmissionDialog } from "../components/review/ReviewSubmissionDialog";
import { InlineCommentComposer } from "../components/review/InlineCommentComposer";
import { PendingCommentCard } from "../components/review/PendingCommentCard";
import { ReviewThreadComponent } from "../components/review/ReviewThread";
import { useOptimisticThreads, type OptimisticReviewThread } from "../review/useOptimisticThreads";
import type { ReviewThread, PendingComment } from "../review/types";

/** Metadata attached to each inline annotation in the diff. */
type AnnotationMeta =
  | { type: "composer" }
  | { type: "pending"; comment: PendingComment }
  | { type: "thread"; thread: ReviewThread | OptimisticReviewThread };

interface Props {
  repo: OpenRepo;
  prNumber: number;
  onClose?: () => void;
}

type TabId = "conversation" | "checks" | "files";

// ── Status badge (GitHub-style pill) ────────────────────────────────

const STATUS_CONFIG: Record<PullRequestStatus, { bg: string; text: string; icon: typeof GitPullRequest; label: string }> = {
  draft: { bg: "bg-accent", text: "text-foreground", icon: GitPullRequest, label: "Draft" },
  open: { bg: "bg-success", text: "text-foreground", icon: GitPullRequest, label: "Open" },
  approved: { bg: "bg-success", text: "text-foreground", icon: CheckCircle2, label: "Open" },
  changes_requested: { bg: "bg-success", text: "text-foreground", icon: GitPullRequest, label: "Open" },
  merged: { bg: "bg-thinking", text: "text-foreground", icon: GitMerge, label: "Merged" },
};

function PrStatusBadge({ status }: { status: PullRequestStatus }) {
  const config = STATUS_CONFIG[status] ?? STATUS_CONFIG.open;
  const Icon = config.icon;
  return (
    <span className={clsx("inline-flex items-center gap-1 rounded-full px-2.5 py-0.5 text-xs font-medium", config.bg, config.text)}>
      <Icon size={12} />
      {config.label}
    </span>
  );
}

// ── Helpers ──────────────────────────────────────────────────────────

function relativeTime(iso: string): string {
  try {
    const d = new Date(iso);
    const diffMs = Date.now() - d.getTime();
    if (diffMs < 60_000) return "just now";
    if (diffMs < 3_600_000) return `${Math.floor(diffMs / 60_000)} minutes ago`;
    if (diffMs < 86_400_000) return `${Math.floor(diffMs / 3_600_000)} hours ago`;
    if (diffMs < 604_800_000) return `${Math.floor(diffMs / 86_400_000)} days ago`;
    return d.toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" });
  } catch {
    return iso;
  }
}

/**
 * Backward compatibility: construct minimal ReviewThread[] from the flat
 * inline_comments structure when the backend hasn't implemented the richer
 * `threads` field yet.
 */
function threadsFromInlineComments(
  inlineComments?: Record<string, Record<number, PrInlineComment[]>>,
): ReviewThread[] {
  if (!inlineComments) return [];
  const threads: ReviewThread[] = [];
  for (const [path, lineMap] of Object.entries(inlineComments)) {
    for (const [lineStr, comments] of Object.entries(lineMap)) {
      const line = Number(lineStr);
      if (!comments.length) continue;
      threads.push({
        threadId: comments[0].id,
        path,
        line,
        resolved: false,
        outdated: false,
        comments: comments.map((c) => ({
          id: c.id,
          author: c.author,
          body: c.body,
          createdAt: c.created_at,
          edited: false,
          isMine: false,
        })),
      });
    }
  }
  return threads;
}


// ── Main view ────────────────────────────────────────────────────────

export function PullRequestView({ repo, prNumber, onClose }: Props) {
  const [detail, setDetail] = useState<PullRequestDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const fetchDetail = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await rpc.forgePrDetail(repo.repo_id, prNumber);
      setDetail(result);
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setError(msg);
    } finally {
      setLoading(false);
    }
  }, [repo.repo_id, prNumber]);

  // Reset state when PR number changes so stale data from the
  // previous PR is never shown while the new one loads.
  useEffect(() => {
    setDetail(null);
    setError(null);
    setLoading(true);
    void fetchDetail();
  }, [fetchDetail]);

  if (loading && !detail) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-muted-foreground">
        <Loader2 size={20} className="animate-spin" />
        <span className="text-sm">Loading PR #{prNumber}…</span>
      </div>
    );
  }

  if (error && !detail) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 px-4 text-center">
        <GitPullRequest size={32} className="text-muted-foreground" />
        <div className="text-sm text-foreground">Unable to load pull request</div>
        <div className="max-w-[300px] text-[11px] text-muted-foreground">{error}</div>
        <Button variant="outline" type="button" onClick={() => void fetchDetail()} className="mt-2 text-xs">Retry</Button>
      </div>
    );
  }

  if (!detail) return null;

  return (
    <PendingReviewProvider repoId={repo.repo_id} prNumber={prNumber}>
      <PullRequestViewInner
        repo={repo}
        prNumber={prNumber}
        detail={detail}
        onClose={onClose}
        onRefresh={fetchDetail}
      />
    </PendingReviewProvider>
  );
}

// ── Inner view (rendered inside PendingReviewProvider) ────────────────

function PullRequestViewInner({ repo, prNumber, detail, onClose, onRefresh }: {
  repo: OpenRepo;
  prNumber: number;
  detail: PullRequestDetail;
  onClose?: () => void;
  onRefresh: () => void;
}) {
  const [activeTab, setActiveTab] = useState<TabId>("conversation");
  const [submissionDialogOpen, setSubmissionDialogOpen] = useState(false);
  const { comments, removeComment, submitReview } = usePendingReview();

  const timeline = detail.timeline ?? [];
  const files = detail.changed_files ?? [];
  const checks = detail.checks ?? [];
  const totalAdditions = files.reduce((s, f) => s + f.additions, 0);
  const totalDeletions = files.reduce((s, f) => s + f.deletions, 0);

  // Derive threads: use detail.threads when available, otherwise construct
  // minimal ReviewThread[] from the flat inline_comments for backward compat.
  const threads: ReviewThread[] = useMemo(() => {
    if (detail.threads) return detail.threads;
    return threadsFromInlineComments(detail.inline_comments);
  }, [detail.threads, detail.inline_comments]);

  const currentUser = detail.current_user ?? "";
  const isAuthor = detail.is_author ?? false;
  const hasWriteAccess = detail.has_write_access ?? false;

  const openSubmissionDialog = useCallback(() => {
    setSubmissionDialogOpen(true);
  }, []);

  const handleSubmitReview = useCallback(
    async (body: string, verdict: import("../review/types").Verdict) => {
      await submitReview(body, verdict);
      onRefresh();
    },
    [submitReview, onRefresh],
  );

  return (
    <div className="flex h-full flex-col bg-background">
      {/* ── Title + branch info (GitHub-style header) ── */}
      <div className="shrink-0 border-b border-border bg-card px-5 pt-4 pb-0">
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0 flex-1">
            <h1 className="text-lg font-semibold text-foreground leading-tight">
              {detail.title}{" "}
              <span className="font-normal text-muted-foreground">#{detail.number}</span>
            </h1>
            <div className="mt-2 flex items-center gap-2 text-xs text-muted-foreground">
              <PrStatusBadge status={detail.status} />
              <span>
                <span className="font-medium text-foreground">{detail.author}</span>
                {" wants to merge into "}
                <span className="rounded bg-info/10 px-1.5 py-0.5 font-mono text-[11px] text-info">
                  {detail.target_branch}
                </span>
                {" from "}
                <span className="rounded bg-info/10 px-1.5 py-0.5 font-mono text-[11px] text-info">
                  {detail.source_branch}
                </span>
              </span>
            </div>
          </div>
          {onClose && (
            <button type="button" onClick={onClose} className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground" aria-label="Close">
              <X size={18} />
            </button>
          )}
        </div>

        {/* ── Tab bar ── */}
        <div className="mt-3 flex items-center gap-0 border-b-0">
          <TabButton id="conversation" active={activeTab} onClick={setActiveTab} icon={MessageSquare} label="Conversation" count={timeline.length} />
          <TabButton id="checks" active={activeTab} onClick={setActiveTab} icon={CheckCircle2} label="Checks" count={checks.length} />
          <TabButton id="files" active={activeTab} onClick={setActiveTab} icon={FileCode2} label="Files changed" count={files.length} badge={<DiffStatMini additions={totalAdditions} deletions={totalDeletions} />} />
        </div>
      </div>

      {/* ── Content area ── */}
      <div className={clsx("min-h-0 flex-1", activeTab === "files" ? "flex flex-col overflow-hidden" : "overflow-auto")}>
        {activeTab === "conversation" && (
          <div className="flex">
            {/* Main conversation column */}
            <div className="min-w-0 flex-1 p-5">
              <ConversationTab
                detail={detail}
                timeline={timeline}
                repo={repo}
                prNumber={prNumber}
                onRefresh={onRefresh}
                onOpenSubmissionDialog={openSubmissionDialog}
              />
            </div>
            {/* Right sidebar */}
            <div className="hidden w-64 shrink-0 border-l border-border p-4 lg:block">
              <PrSidebar
                detail={detail}
                repoId={repo.repo_id}
                prNumber={prNumber}
                isAuthor={isAuthor}
                hasWriteAccess={hasWriteAccess}
              />
            </div>
          </div>
        )}
        {activeTab === "checks" && (
          <div className="p-5">
            <ChecksTab checks={checks} />
          </div>
        )}
        {activeTab === "files" && (
          <FilesTab
            files={files}
            repo={repo}
            inlineComments={detail.inline_comments}
            prNumber={prNumber}
            onRefresh={onRefresh}
            baseBranch={detail.target_branch}
            headBranch={detail.source_branch}
            threads={threads}
            currentUser={currentUser}
            isAuthor={isAuthor}
            hasWriteAccess={hasWriteAccess}
            headSha={detail.head_sha ?? ""}
          />
        )}
      </div>

      {/* Floating pending review badge — visible from all tabs */}
      <PendingReviewFab onClick={openSubmissionDialog} />

      {/* Review submission dialog */}
      <ReviewSubmissionDialog
        open={submissionDialogOpen}
        onClose={() => setSubmissionDialogOpen(false)}
        pendingComments={comments}
        onRemoveComment={removeComment}
        onSubmit={handleSubmitReview}
        isAuthor={isAuthor}
      />
    </div>
  );
}

// ── Tab button ───────────────────────────────────────────────────────

function TabButton({ id, active, onClick, icon: Icon, label, count, badge }: {
  id: TabId;
  active: TabId;
  onClick: (id: TabId) => void;
  icon: typeof MessageSquare;
  label: string;
  count?: number;
  badge?: React.ReactNode;
}) {
  const isActive = active === id;
  return (
    <button
      type="button"
      onClick={() => onClick(id)}
      className={clsx(
        "relative flex items-center gap-1.5 px-3 py-2 text-xs font-medium transition-colors",
        isActive ? "text-foreground" : "text-muted-foreground hover:text-foreground",
      )}
    >
      <Icon size={14} />
      {label}
      {count != null && count > 0 && (
        <span className={clsx("rounded-full px-1.5 py-[1px] text-[10px]", isActive ? "bg-primary/20 text-primary" : "bg-background text-muted-foreground")}>
          {count}
        </span>
      )}
      {badge}
      {isActive && <span className="absolute bottom-0 left-0 right-0 h-[2px] rounded-t bg-primary" />}
    </button>
  );
}

function DiffStatMini({ additions, deletions }: { additions: number; deletions: number }) {
  if (!additions && !deletions) return null;
  return (
    <span className="ml-1 inline-flex items-center gap-0.5 text-[10px]">
      <span className="text-success">+{additions.toLocaleString()}</span>
      <span className="text-destructive">-{deletions.toLocaleString()}</span>
    </span>
  );
}


// ── Right sidebar (GitHub-style metadata) ────────────────────────────

function PrSidebar({ detail, repoId, prNumber, isAuthor, hasWriteAccess }: {
  detail: PullRequestDetail;
  repoId: string;
  prNumber: number;
  isAuthor: boolean;
  hasWriteAccess: boolean;
}) {
  const reviews = (detail.timeline ?? []).filter((e) => e.kind === "review");
  const reviewers = [...new Set(reviews.map((r) => r.author))];

  return (
    <div className="space-y-5 text-xs">
      {/* Reviewers */}
      <SidebarSection icon={Users} title="Reviewers">
        {reviewers.length === 0 ? (
          <span className="text-muted-foreground">No reviewers yet</span>
        ) : (
          <div className="space-y-1.5">
            {reviewers.map((name) => {
              const latestReview = [...reviews].reverse().find((r) => r.author === name);
              return (
                <ReviewerRow
                  key={name}
                  name={name}
                  verdict={latestReview?.verdict}
                  repoId={repoId}
                  prNumber={prNumber}
                  canReRequest={isAuthor || hasWriteAccess}
                />
              );
            })}
          </div>
        )}
      </SidebarSection>

      {/* Assignees */}
      <SidebarSection icon={User} title="Assignees">
        <span className="text-muted-foreground">No one assigned</span>
      </SidebarSection>

      {/* Labels */}
      <SidebarSection icon={Tag} title="Labels">
        {(!detail.labels || detail.labels.length === 0) ? (
          <span className="text-muted-foreground">None yet</span>
        ) : (
          <div className="flex flex-wrap gap-1">
            {detail.labels.map((label) => (
              <span key={label} className="rounded-full border border-border bg-background px-2 py-[2px] text-[10px] font-medium text-foreground">
                {label}
              </span>
            ))}
          </div>
        )}
      </SidebarSection>

      {/* Merge status */}
      {detail.mergeable != null && (
        <SidebarSection icon={GitMerge} title="Merge status">
          {detail.mergeable ? (
            <span className="flex items-center gap-1 text-success">
              <CheckCircle2 size={11} /> Ready to merge
            </span>
          ) : (
            <span className="flex items-center gap-1 text-warning">
              <XCircle size={11} /> Conflicts detected
            </span>
          )}
        </SidebarSection>
      )}
    </div>
  );
}

function ReviewerRow({ name, verdict, repoId, prNumber, canReRequest }: {
  name: string;
  verdict?: ReviewVerdict;
  repoId: string;
  prNumber: number;
  canReRequest: boolean;
}) {
  const [status, setStatus] = useState<"idle" | "loading" | "success" | "error">("idle");
  const [errorMsg, setErrorMsg] = useState<string | null>(null);

  const handleReRequest = async () => {
    setStatus("loading");
    setErrorMsg(null);
    try {
      await rpc.forgeReRequestReview(repoId, prNumber, name);
      setStatus("success");
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setErrorMsg(msg);
      setStatus("error");
    }
  };

  const verdictIcon = () => {
    if (status === "success") return <Clock size={12} className="text-muted-foreground" />;
    if (verdict === "approved") return <CheckCircle2 size={12} className="text-success" />;
    if (verdict === "changes_requested") return <XCircle size={12} className="text-warning" />;
    if (verdict === "commented") return <Eye size={12} className="text-muted-foreground" />;
    return null;
  };

  return (
    <div>
      <div className="flex items-center gap-2">
        <div className="flex h-5 w-5 items-center justify-center rounded-full bg-primary/20 text-[9px] font-bold text-primary">
          {name[0]?.toUpperCase()}
        </div>
        <span className="flex-1 truncate text-foreground">{name}</span>
        {verdictIcon()}
        {canReRequest && status !== "success" && (
          <button
            type="button"
            onClick={() => void handleReRequest()}
            disabled={status === "loading"}
            className="rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground disabled:opacity-50"
            aria-label={`Re-request review from ${name}`}
            title="Re-request review"
          >
            {status === "loading" ? (
              <Loader2 size={12} className="animate-spin" />
            ) : (
              <RefreshCw size={12} />
            )}
          </button>
        )}
      </div>
      {status === "error" && errorMsg && (
        <div className="ml-7 mt-0.5 text-[10px] text-destructive">{errorMsg}</div>
      )}
    </div>
  );
}

function SidebarSection({ icon: Icon, title, children }: { icon: typeof Users; title: string; children: React.ReactNode }) {
  return (
    <div>
      <div className="mb-2 flex items-center gap-1.5 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
        <Icon size={12} />
        {title}
      </div>
      {children}
    </div>
  );
}

// ── Conversation tab ─────────────────────────────────────────────────

function ConversationTab({ detail, timeline, repo, prNumber, onRefresh, onOpenSubmissionDialog }: {
  detail: PullRequestDetail;
  timeline: PrTimelineEvent[];
  repo: OpenRepo;
  prNumber: number;
  onRefresh: () => void;
  onOpenSubmissionDialog: () => void;
}) {
  const [commentBody, setCommentBody] = useState("");
  const [posting, setPosting] = useState(false);

  const postComment = async () => {
    if (!commentBody.trim()) return;
    setPosting(true);
    try {
      await rpc.forgePrComment(repo.repo_id, prNumber, commentBody.trim());
      setCommentBody("");
      onRefresh();
    } catch { /* toast */ } finally { setPosting(false); }
  };

  return (
    <div className="space-y-4">
      {/* PR body as first "comment" */}
      {detail.body && (
        <CommentCard author={detail.author} createdAt={detail.created_at} body={detail.body} isOp />
      )}

      {/* Timeline events */}
      {timeline.map((event) => (
        <TimelineEventCard key={event.id} event={event} />
      ))}

      {/* Comment input */}
      <div className="rounded-lg border border-border bg-card">
        <div className="border-b border-border px-3 py-2 text-[11px] font-medium text-muted-foreground">
          Write
        </div>
        <div className="p-3">
          <Textarea
            className="w-full resize-none rounded border border-border bg-background p-3 text-sm text-foreground placeholder:text-muted-foreground focus:border-primary focus-visible:outline-none"
            rows={4}
            placeholder="Leave a comment…"
            value={commentBody}
            onChange={(e) => setCommentBody(e.target.value)}
          />
        </div>
        <div className="flex items-center justify-end gap-2 border-t border-border px-3 py-2">
          <ReviewDropdown repo={repo} prNumber={prNumber} onRefresh={onRefresh} onOpenSubmissionDialog={onOpenSubmissionDialog} />
          <button
            type="button"
            className="inline-flex items-center gap-1.5 rounded-md bg-success px-3 py-1.5 text-xs font-medium text-success-foreground hover:bg-success disabled:opacity-50"
            disabled={!commentBody.trim() || posting}
            onClick={() => void postComment()}
          >
            {posting ? <Loader2 size={12} className="animate-spin" /> : <Send size={12} />}
            Comment
          </button>
        </div>
      </div>
    </div>
  );
}

function CommentCard({ author, createdAt, body, isOp }: { author: string; createdAt: string; body: string; isOp?: boolean }) {
  return (
    <div className="rounded-lg border border-border bg-card overflow-hidden">
      <div className={clsx("flex items-center gap-2 px-4 py-2 text-xs", isOp ? "bg-info/5 border-b border-info/20" : "bg-background border-b border-border")}>
        <div className="flex h-5 w-5 items-center justify-center rounded-full bg-primary/20 text-[9px] font-bold text-primary">
          {author[0]?.toUpperCase()}
        </div>
        <span className="font-semibold text-foreground">{author}</span>
        {isOp && <span className="rounded border border-border px-1 py-[1px] text-[9px] text-muted-foreground">Author</span>}
        <span className="text-muted-foreground">commented {relativeTime(createdAt)}</span>
      </div>
      <div className="p-4 text-sm text-foreground leading-relaxed prose-agent">
        <Streamdown shikiTheme={["github-light", "github-dark"]}>
          {body}
        </Streamdown>
      </div>
    </div>
  );
}

function TimelineEventCard({ event }: { event: PrTimelineEvent }) {
  if (event.kind === "comment") {
    return <CommentCard author={event.author} createdAt={event.created_at} body={event.body ?? ""} />;
  }

  if (event.kind === "review") {
    return (
      <div className="flex items-start gap-3 py-2">
        <div className="mt-0.5">
          <VerdictIcon verdict={event.verdict} />
        </div>
        <div className="text-xs">
          <span className="font-semibold text-foreground">{event.author}</span>{" "}
          <VerdictLabel verdict={event.verdict} />{" "}
          <span className="text-muted-foreground">{relativeTime(event.created_at)}</span>
          {event.body && (
            <div className="mt-1 rounded border border-border bg-card p-3 text-sm text-foreground leading-relaxed prose-agent">
              <Streamdown shikiTheme={["github-light", "github-dark"]}>
                {event.body}
              </Streamdown>
            </div>
          )}
        </div>
      </div>
    );
  }

  if (event.kind === "commit") {
    return (
      <div className="flex items-center gap-3 py-1.5 text-xs">
        <GitCommitHorizontal size={14} className="text-muted-foreground" />
        <span className="font-semibold text-foreground">{event.author}</span>
        <span className="text-muted-foreground">pushed a commit</span>
        {event.sha && <span className="font-mono text-[10px] text-muted-foreground">{event.sha.slice(0, 7)}</span>}
        {event.subject && <span className="truncate text-foreground">{event.subject}</span>}
      </div>
    );
  }

  return null;
}

function VerdictIcon({ verdict }: { verdict?: ReviewVerdict }) {
  if (verdict === "approved") return <CheckCircle2 size={16} className="text-success" />;
  if (verdict === "changes_requested") return <ShieldAlert size={16} className="text-warning" />;
  return <Eye size={16} className="text-muted-foreground" />;
}

function VerdictLabel({ verdict }: { verdict?: ReviewVerdict }) {
  if (verdict === "approved") return <span className="font-medium text-success">approved these changes</span>;
  if (verdict === "changes_requested") return <span className="font-medium text-warning">requested changes</span>;
  return <span className="text-muted-foreground">left a review</span>;
}

function ReviewDropdown({ repo, prNumber, onRefresh, onOpenSubmissionDialog }: { repo: OpenRepo; prNumber: number; onRefresh: () => void; onOpenSubmissionDialog: () => void }) {
  const [open, setOpen] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const { count } = usePendingReview();

  const handleClick = () => {
    // When there are pending comments, open the submission dialog instead
    // of the simple approve/request-changes dropdown.
    if (count > 0) {
      onOpenSubmissionDialog();
      return;
    }
    setOpen(!open);
  };

  const submit = async (event: "approve" | "request_changes") => {
    setSubmitting(true);
    try {
      await rpc.forgeSubmitReview(repo.repo_id, prNumber, { body: "", event });
      onRefresh();
    } catch { /* toast */ } finally { setSubmitting(false); setOpen(false); }
  };

  return (
    <div className="relative">
      <button type="button" onClick={handleClick} className="inline-flex items-center gap-1 rounded-md border border-border px-2.5 py-1.5 text-xs text-foreground hover:bg-accent">
        Review {count > 0 && <span className="rounded-full bg-warning/20 px-1.5 py-[1px] text-[10px] text-warning">{count}</span>} <ChevronDown size={10} />
      </button>
      {open && (
        <div className="absolute bottom-full right-0 mb-1 w-48 rounded-lg border border-border bg-card p-1 shadow-lg">
          <button type="button" disabled={submitting} onClick={() => void submit("approve")} className="flex w-full items-center gap-2 rounded px-3 py-2 text-xs text-foreground hover:bg-accent">
            <CheckCircle2 size={14} className="text-success" /> Approve
          </button>
          <button type="button" disabled={submitting} onClick={() => void submit("request_changes")} className="flex w-full items-center gap-2 rounded px-3 py-2 text-xs text-foreground hover:bg-accent">
            <ShieldAlert size={14} className="text-warning" /> Request changes
          </button>
        </div>
      )}
    </div>
  );
}


// ── Checks tab ───────────────────────────────────────────────────────

function ChecksTab({ checks }: { checks: import("../ipc").CiCheck[] }) {
  const status = aggregateCheckStatus(checks);
  const passed = checks.filter((c) => c.status === "completed" && (c.conclusion === "success" || c.conclusion === "neutral" || c.conclusion === "skipped")).length;

  if (checks.length === 0) {
    return (
      <div className="flex flex-col items-center justify-center py-12 text-muted-foreground">
        <CheckCircle2 size={32} className="mb-2 opacity-40" />
        <span className="text-sm">No checks configured for this pull request</span>
      </div>
    );
  }

  return (
    <div className="mx-auto max-w-3xl">
      {/* Summary bar */}
      <div className={clsx("mb-4 flex items-center gap-3 rounded-lg border p-4 text-sm", status === "success" ? "border-success/30 bg-success/5" : status === "failure" ? "border-destructive/30 bg-destructive/5" : "border-warning/30 bg-warning/5")}>
        {status === "success" && <CheckCircle2 size={20} className="text-success" />}
        {status === "failure" && <XCircle size={20} className="text-destructive" />}
        {(status === "running" || status === "pending") && <Clock size={20} className="text-warning" />}
        <span className="flex-1 text-foreground font-medium">
          {status === "success" && "All checks have passed"}
          {status === "failure" && "Some checks were not successful"}
          {status === "running" && "Some checks are still running"}
          {status === "pending" && "Some checks are pending"}
        </span>
        <span className="text-xs text-muted-foreground">{passed} / {checks.length} successful</span>
      </div>

      {/* Check list */}
      <div className="rounded-lg border border-border bg-card divide-y divide-border">
        {checks.map((check) => (
          <div key={check.name} className="flex items-center gap-3 px-4 py-3">
            <CheckIcon check={check} />
            <span className="min-w-0 flex-1 truncate text-sm text-foreground">{check.name}</span>
            <span className="text-xs text-muted-foreground">
              {check.status === "completed" ? (check.conclusion ?? "done") : check.status}
            </span>
            {check.url && (
              <button type="button" onClick={() => window.open(check.url!, "_blank", "noopener")} className="text-[10px] text-primary hover:underline">
                Details
              </button>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}

function CheckIcon({ check }: { check: import("../ipc").CiCheck }) {
  if (check.status === "queued") return <Clock size={16} className="text-warning" />;
  if (check.status === "in_progress") return <Loader2 size={16} className="animate-spin text-warning" />;
  if (check.conclusion === "success" || check.conclusion === "neutral" || check.conclusion === "skipped") return <CheckCircle2 size={16} className="text-success" />;
  return <XCircle size={16} className="text-destructive" />;
}

// ── Files changed tab ────────────────────────────────────────────────

function FilesTab({ files, repo, inlineComments, prNumber, onRefresh, baseBranch, headBranch, threads, currentUser, isAuthor: _isAuthor, hasWriteAccess, headSha }: {
  files: PrChangedFile[];
  repo: OpenRepo;
  inlineComments?: Record<string, Record<number, PrInlineComment[]>>;
  prNumber: number;
  onRefresh: () => void;
  baseBranch?: string;
  headBranch?: string;
  threads: ReviewThread[];
  currentUser: string;
  isAuthor: boolean;
  hasWriteAccess: boolean;
  headSha: string;
}) {
  if (files.length === 0) {
    return (
      <div className="flex flex-col items-center justify-center py-12 text-muted-foreground">
        <FileCode2 size={32} className="mb-2 opacity-40" />
        <span className="text-sm">No files changed</span>
      </div>
    );
  }

  return (
    <FileProgressProvider repoId={repo.repo_id} prNumber={prNumber} files={files} headSha={headSha}>
      <FilesTabInner
        files={files}
        repo={repo}
        inlineComments={inlineComments}
        prNumber={prNumber}
        onRefresh={onRefresh}
        baseBranch={baseBranch}
        headBranch={headBranch}
        threads={threads}
        currentUser={currentUser}
        hasWriteAccess={hasWriteAccess}
      />
    </FileProgressProvider>
  );
}

function FilesTabInner({ files, repo, inlineComments, prNumber, onRefresh, baseBranch, headBranch, threads, currentUser, hasWriteAccess }: {
  files: PrChangedFile[];
  repo: OpenRepo;
  inlineComments?: Record<string, Record<number, PrInlineComment[]>>;
  prNumber: number;
  onRefresh: () => void;
  baseBranch?: string;
  headBranch?: string;
  threads: ReviewThread[];
  currentUser: string;
  hasWriteAccess: boolean;
}) {
  const [activeFile, setActiveFile] = useState<string | null>(
    files.length > 0 ? files[0].path : null,
  );
  const effective = useEffectiveTheme();
  const totalAdditions = files.reduce((s, f) => s + f.additions, 0);
  const totalDeletions = files.reduce((s, f) => s + f.deletions, 0);
  const scrollRef = useRef<HTMLDivElement>(null);
  const fileRefs = useRef<Map<string, HTMLDivElement>>(new Map());
  const { markViewed, markUnviewed, isViewed, viewedCount, totalFiles } = useFileProgress();
  const { comments: pendingComments, addComment, updateComment, removeComment } = usePendingReview();

  // Composer state: which file/line has the comment composer open
  const [composerState, setComposerState] = useState<{
    path: string;
    line: number;
    side: "LEFT" | "RIGHT";
    endLine?: number;
  } | null>(null);

  // Optimistic threads for reply/resolve/edit/delete
  const {
    threads: optimisticThreads,
    replyToThread,
    retryReply,
    dismissFailedReply,
    resolveThread,
    dismissResolveError,
    editComment: editThreadComment,
    deleteComment: deleteThreadComment,
  } = useOptimisticThreads(threads, repo.repo_id, prNumber, currentUser);

  // Group optimistic threads by file path
  const threadsByFile = useMemo(() => {
    const map = new Map<string, typeof optimisticThreads>();
    for (const thread of optimisticThreads) {
      const existing = map.get(thread.path) ?? [];
      existing.push(thread);
      map.set(thread.path, existing);
    }
    return map;
  }, [optimisticThreads]);

  // Group pending comments by file path
  const pendingCommentsByFile = useMemo(() => {
    const map = new Map<string, typeof pendingComments>();
    for (const comment of pendingComments) {
      const existing = map.get(comment.path) ?? [];
      existing.push(comment);
      map.set(comment.path, existing);
    }
    return map;
  }, [pendingComments]);

  // Callback for the comment button
  const handleCommentLine = useCallback((path: string, line: number, side: "LEFT" | "RIGHT") => {
    setComposerState({ path, line, side });
  }, []);

  // Callback for multi-line range selection (gutter drag or line selection)
  const handleCommentRange = useCallback((path: string, startLine: number, endLine: number, side: "LEFT" | "RIGHT") => {
    setComposerState({ path, line: startLine, endLine, side });
  }, []);

  const handleComposerSubmit = useCallback((body: string) => {
    if (!composerState) return;
    addComment({
      path: composerState.path,
      line: composerState.line,
      endLine: composerState.endLine ?? null,
      side: composerState.side,
      body,
    });
    setComposerState(null);
  }, [composerState, addComment]);

  const handleComposerCancel = useCallback(() => {
    setComposerState(null);
  }, []);

  // Build line annotations for a specific file, combining:
  // - pending draft comments
  // - published review threads
  // - the active composer position
  const buildAnnotationsForFile = useCallback((filePath: string): DiffLineAnnotation<AnnotationMeta>[] => {
    const annotations: DiffLineAnnotation<AnnotationMeta>[] = [];

    // Pending comments
    const filePending = pendingCommentsByFile.get(filePath);
    if (filePending) {
      for (const comment of filePending) {
        annotations.push({
          side: comment.side === "LEFT" ? "deletions" : "additions",
          lineNumber: comment.line,
          metadata: { type: "pending", comment },
        });
      }
    }

    // Published threads
    const fileThreads = threadsByFile.get(filePath);
    if (fileThreads) {
      for (const thread of fileThreads) {
        // Default to additions side since threads from the API don't always have side info
        annotations.push({
          side: "additions",
          lineNumber: thread.line,
          metadata: { type: "thread", thread },
        });
      }
    }

    // Active composer — anchored at the END line of the selection so it
    // appears below the highlighted range (matching GitHub's behavior).
    if (composerState?.path === filePath) {
      annotations.push({
        side: composerState.side === "LEFT" ? "deletions" : "additions",
        lineNumber: composerState.endLine ?? composerState.line,
        metadata: { type: "composer" },
      });
    }

    return annotations;
  }, [pendingCommentsByFile, threadsByFile, composerState]);

  // Render annotation content based on metadata type
  const renderAnnotationContent = useCallback((annotation: DiffLineAnnotation<AnnotationMeta>, filePath: string) => {
    const meta = annotation.metadata;
    if (!meta) return null;

    if (meta.type === "composer" && composerState?.path === filePath) {
      return (
        <InlineCommentComposer
          path={composerState.path}
          line={composerState.line}
          endLine={composerState.endLine}
          side={composerState.side}
          codeContext=""
          onSubmit={handleComposerSubmit}
          onCancel={handleComposerCancel}
        />
      );
    }

    if (meta.type === "pending") {
      return (
        <PendingCommentCard
          comment={meta.comment}
          onUpdate={updateComment}
          onRemove={removeComment}
        />
      );
    }

    if (meta.type === "thread") {
      return (
        <ReviewThreadComponent
          thread={meta.thread}
          currentUser={currentUser}
          hasWriteAccess={hasWriteAccess}
          onReply={replyToThread}
          onResolve={resolveThread}
          onEditComment={editThreadComment}
          onDeleteComment={deleteThreadComment}
          onApplySuggestion={async () => {}}
          onRetryReply={retryReply}
          onDismissFailedReply={dismissFailedReply}
          onDismissResolveError={dismissResolveError}
        />
      );
    }

    return null;
  }, [composerState, handleComposerSubmit, handleComposerCancel, updateComment, removeComment, currentUser, hasWriteAccess, replyToThread, resolveThread, editThreadComment, deleteThreadComment, retryReply, dismissFailedReply, dismissResolveError]);

  // Build diff options with gutter utility (GitHub-style "+" button on hover)
  const reviewDiffOptions = useCallback((filePath: string) => {
    const base = prDiffOptions(effective);
    return {
      ...base,
      enableGutterUtility: true,
      enableLineSelection: true,
      onGutterUtilityClick: (range: SelectedLineRange) => {
        const side = range.side === "deletions" ? "LEFT" as const : "RIGHT" as const;
        if (range.start === range.end) {
          handleCommentLine(filePath, range.start, side);
        } else {
          handleCommentRange(filePath, range.start, range.end, side);
        }
      },
      onLineSelected: (range: SelectedLineRange | null) => {
        if (!range) return;
        const side = range.side === "deletions" ? "LEFT" as const : "RIGHT" as const;
        if (range.start === range.end) {
          handleCommentLine(filePath, range.start, side);
        } else {
          handleCommentRange(filePath, range.start, range.end, side);
        }
      },
    };
  }, [effective, handleCommentLine, handleCommentRange]);

  const scrollToFile = useCallback((path: string) => {
    setActiveFile(path);
    const el = fileRefs.current.get(path);
    if (el && scrollRef.current) {
      el.scrollIntoView({ behavior: "smooth", block: "start" });
    }
  }, []);

  const setFileRef = useCallback((path: string, el: HTMLDivElement | null) => {
    if (el) fileRefs.current.set(path, el);
    else fileRefs.current.delete(path);
  }, []);

  const handleAddComment = useCallback(
    async (_filePath: string, _line: number, body: string) => {
      try { await rpc.forgePrComment(repo.repo_id, prNumber, body); onRefresh(); } catch { /* */ }
    },
    [repo.repo_id, prNumber, onRefresh],
  );

  // Determine which files have all threads resolved (for checkmark icon).
  const allThreadsResolvedByFile = useMemo(() => {
    const result = new Map<string, boolean>();
    // Group threads by file path.
    const threadsByFile = new Map<string, ReviewThread[]>();
    for (const thread of threads) {
      const existing = threadsByFile.get(thread.path) ?? [];
      existing.push(thread);
      threadsByFile.set(thread.path, existing);
    }
    // A file shows a checkmark if it has threads AND all are resolved.
    for (const [path, fileThreads] of threadsByFile) {
      result.set(path, fileThreads.length > 0 && fileThreads.every((t) => t.resolved));
    }
    return result;
  }, [threads]);

  return (
    <div className="flex h-full min-h-0 flex-col">
      {/* Sticky summary header */}
      <div className="shrink-0 flex items-center justify-between border-b border-border bg-card px-4 py-2.5">
        <div className="flex items-center gap-4 text-xs">
          <span className="font-medium text-foreground">{files.length} files changed</span>
          <span className="flex items-center gap-1.5">
            <span className="font-semibold text-success">+{totalAdditions.toLocaleString()}</span>
            <span className="font-semibold text-destructive">-{totalDeletions.toLocaleString()}</span>
          </span>
          <DiffBar additions={totalAdditions} deletions={totalDeletions} />
        </div>
        {/* File progress indicator */}
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <Eye size={12} />
          <span>{viewedCount}/{totalFiles} files viewed</span>
        </div>
      </div>

      {/* Two-pane layout: file sidebar + diffs */}
      <div className="grid min-h-0 flex-1 grid-cols-[280px_1fr]">
        {/* File sidebar */}
        <aside className="min-h-0 overflow-auto border-r border-border bg-card">
          <ul className="py-1">
            {files.map((file) => {
              const isActive = file.path === activeFile;
              const fileComments = inlineComments?.[file.path];
              const commentCount = fileComments ? Object.values(fileComments).reduce((sum, arr) => sum + arr.length, 0) : 0;
              const viewed = isViewed(file.path);
              const allResolved = allThreadsResolvedByFile.get(file.path) === true;
              return (
                <li key={file.path}>
                  <div
                    className={clsx(
                      "flex w-full items-center gap-2 px-3 py-1.5 text-[11px] transition",
                      isActive
                        ? "bg-primary/15 text-foreground"
                        : "text-foreground hover:bg-accent",
                      viewed && "opacity-60",
                    )}
                  >
                    {/* Viewed checkbox */}
                    <input
                      type="checkbox"
                      checked={viewed}
                      onChange={(e) => {
                        e.stopPropagation();
                        if (viewed) markUnviewed(file.path);
                        else markViewed(file.path);
                      }}
                      className="h-3 w-3 shrink-0 cursor-pointer rounded border-border accent-primary"
                      title={viewed ? "Mark as unviewed" : "Mark as viewed"}
                      aria-label={viewed ? `Mark ${file.path} as unviewed` : `Mark ${file.path} as viewed`}
                    />
                    <button
                      type="button"
                      onClick={() => scrollToFile(file.path)}
                      className="flex min-w-0 flex-1 items-center gap-2 text-left"
                      title={file.path}
                    >
                      <FileCode2 size={11} className="shrink-0 text-muted-foreground" />
                      <span className="min-w-0 flex-1 truncate font-mono">
                        {file.path}
                      </span>
                    </button>
                    <span className="shrink-0 flex items-center gap-1 text-[10px]">
                      {allResolved && (
                        <span title="All threads resolved">
                          <CheckCircle2 size={11} className="text-success" />
                        </span>
                      )}
                      {commentCount > 0 && (
                        <span className="flex items-center gap-0.5 text-primary">
                          <MessageSquare size={9} />
                          <span>{commentCount}</span>
                        </span>
                      )}
                      {file.additions > 0 && (
                        <span className="text-success">+{file.additions}</span>
                      )}
                      {file.deletions > 0 && (
                        <span className="text-destructive">-{file.deletions}</span>
                      )}
                    </span>
                  </div>
                </li>
              );
            })}
          </ul>
        </aside>

        {/* Scrollable all-diffs pane */}
        <div ref={scrollRef} className="min-h-0 overflow-auto bg-background">
          {files.map((file) => {
            const fileComments = inlineComments?.[file.path];
            const commentsByLine: CommentsByLine | undefined = fileComments
              ? Object.fromEntries(Object.entries(fileComments).map(([line, comments]) => [Number(line), comments.map((c) => ({ id: c.id, author: c.author, body: c.body, created_at: c.created_at }))]))
              : undefined;
            return (
              <div
                key={file.path}
                ref={(el) => setFileRef(file.path, el)}
                className="border-b border-border"
              >
                {/* Sticky file header */}
                <div
                  className={clsx(
                    "sticky top-0 z-10 flex w-full items-center gap-2 border-b border-border/60 px-4 py-2 text-left backdrop-blur-sm transition",
                    file.path === activeFile
                      ? "bg-primary/10"
                      : "bg-card/95 hover:bg-accent/95",
                  )}
                >
                  <button
                    type="button"
                    onClick={() => setActiveFile(file.path)}
                    className="flex min-w-0 flex-1 items-center gap-2"
                  >
                    <FileCode2 size={12} className="shrink-0 text-muted-foreground" />
                    <span className="min-w-0 flex-1 truncate font-mono text-[12px] font-medium text-foreground">
                      {file.path}
                    </span>
                  </button>
                  <span className="flex items-center gap-2 text-[10px] text-muted-foreground">
                    {file.additions > 0 && (
                      <span className="font-semibold text-success">+{file.additions}</span>
                    )}
                    {file.deletions > 0 && (
                      <span className="font-semibold text-destructive">-{file.deletions}</span>
                    )}
                  </span>
                  <DiffBar additions={file.additions} deletions={file.deletions} />
                </div>

                {/* Diff content with inline annotations */}
                <div className="relative min-w-0">
                  {file.patch ? (
                    <SafePatchDiff
                      patch={normalizePrPatch(file.patch, file.path, file.status)}
                      className="gx-patch-diff"
                      options={reviewDiffOptions(file.path)}
                      lineAnnotations={buildAnnotationsForFile(file.path)}
                      renderAnnotation={(annotation) => renderAnnotationContent(annotation as DiffLineAnnotation<AnnotationMeta>, file.path)}
                    />
                  ) : (
                    <DiffView repo={repo} path={file.path} base={baseBranch} head={headBranch} commentsByLine={commentsByLine} onAddComment={(line, body) => void handleAddComment(file.path, line, body)} />
                  )}
                </div>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}

/** GitHub-style colored diff bar (5 squares showing add/delete ratio). */
function DiffBar({ additions, deletions }: { additions: number; deletions: number }) {
  const total = additions + deletions;
  if (total === 0) return null;
  const addBlocks = Math.round((additions / total) * 5);
  const delBlocks = 5 - addBlocks;
  return (
    <span className="inline-flex gap-[2px]">
      {Array.from({ length: addBlocks }, (_, i) => (
        <span key={`a${i}`} className="h-2 w-2 rounded-[2px] bg-success" />
      ))}
      {Array.from({ length: delBlocks }, (_, i) => (
        <span key={`d${i}`} className="h-2 w-2 rounded-[2px] bg-destructive" />
      ))}
    </span>
  );
}

/**
 * Diff options tailored for the PR "Files changed" view.
 *
 * Uses unified (inline) layout to avoid horizontal overflow in the
 * card container. Disables the internal file header since we render
 * our own sticky header with path, stats, and collapse toggle.
 * Wraps long lines and ensures text is selectable for copy-paste.
 */
function prDiffOptions(effective: "dark" | "light") {
  const base = diffOptions(effective);
  return {
    ...base,
    disableFileHeader: true,
    diffStyle: "unified" as const,
    overflow: "wrap" as const,
    unsafeCSS: `${base.unsafeCSS ?? ""}
      * {
        user-select: text;
        -webkit-user-select: text;
      }
      [data-header], [data-header-content] {
        display: none !important;
      }
      :host {
        border-radius: 0;
      }
    `,
  };
}

/**
 * Normalizes a patch from the GitHub/forge API into a full unified diff
 * that `<PatchDiff />` from @pierre/diffs can render.
 *
 * The GitHub API returns the `patch` field for each file as just the hunk
 * content (starting with `@@`) without the `diff --git a/... b/...` header.
 * PatchDiff requires the full header to parse and render correctly.
 */
function normalizePrPatch(patch: string, filePath: string, status?: string): string {
  if (!patch || !patch.trim()) return patch;
  // Already has the full diff --git header — pass through unchanged.
  if (patch.trimStart().startsWith("diff --git ")) return patch;
  // Build the appropriate --- / +++ lines based on file status.
  const oldPath = status === "added" ? "/dev/null" : `a/${filePath}`;
  const newPath = status === "removed" ? "/dev/null" : `b/${filePath}`;
  const header = [
    `diff --git a/${filePath} b/${filePath}`,
    `--- ${oldPath}`,
    `+++ ${newPath}`,
  ].join("\n");
  return `${header}\n${patch}`;
}
