import {
  ConversationLockControl,
  useConversationLock,
} from "@/components/discussion/conversation/lock";
import { mergeMethod } from "./merge-methods";
import { useCallback, useEffect, useState } from "react";
import { OverlayVisibility } from "@/components/ui/use-overlay-state";
import { useSubmission } from "@/components/discussion/hooks";
import { Author } from "@/components/discussion/author";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { PullChecks } from "./candidate-checks";
import { ChangeBase } from "./change-base";
import { EditPullRequest } from "./edit";
import type { CodeThemes } from "@/lib/code-theme";
import { DiscussionMetadata } from "@/components/discussion/metadata";
import { repoHref, repositoryRoute } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";
import { Result, RequestWidget } from "@/components/request/result";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  ChangeComparison,
  usePullComparison,
} from "@/features/repository/diff";
import { short } from "@/lib/format";
import {
  ChecklistIcon,
  CommentIcon,
  GitCommitIcon,
} from "@primer/octicons-react";

import { PullCommits } from "./commits";
import { PullCodeOwners } from "./codeowners";
import { PullConversation } from "./conversation";
import { useCommentSubmission } from "@/components/discussion/conversation/compose";
import type { PullRequest } from "./model";
import { branch, pullIcons, pullStatus } from "./presentation";
import { ReviewChanges, type ReviewDraft } from "./reviews/form";
import { PendingComments, usePendingReview } from "./reviews/pending";
import { PullReviewThreads } from "./reviews/threads";
import { LinkedReviewThread } from "./reviews/thread-card";

function PullBadge({ pull }: { pull: PullRequest }) {
  const status = pullStatus(pull);
  const Icon = pullIcons[status];
  return (
    <span className={`pull-state ${status}`} aria-live="polite">
      <Icon />
      {status[0].toUpperCase() + status.slice(1)}
    </span>
  );
}

function PublicPullFiles({
  repo,
  pull,
  theme,
  codeThemes,
}: {
  repo: Repository;
  pull: PullRequest;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
}) {
  const comparison = usePullComparison(repo, pull.base_oid, pull.head_oid);
  return (
    <ChangeComparison
      state={comparison}
      repo={repo}
      base={comparison.data?.base}
      rev={comparison.data?.commit ?? ""}
      theme={theme}
      codeThemes={codeThemes}
    />
  );
}

export function PullDetail({
  repo,
  number,
  url,
  csrf,
  theme,
  codeThemes,
}: {
  repo: Repository;
  number: number;
  url: URL;
  csrf: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
}) {
  const conversationLock = useConversationLock();
  const [reviewDraft, setReviewDraft] = useState<ReviewDraft>();
  const reviewSubmission = useSubmission();
  // Draft and submission identity outlive tab changes and metadata refreshes.
  const comment = useCommentSubmission(repo, `pulls/${number}`, csrf);
  const path = endpoint(repo, `pulls/${number}`);
  const pull = useRequest<PullRequest>(path);
  const [current, setCurrent] = useState<PullRequest>();
  const [epoch, setEpoch] = useState(0);
  const accept = useCallback((incoming: PullRequest) => {
    setCurrent((old) =>
      old && old.version > incoming.version ? old : incoming,
    );
  }, []);
  useEffect(() => {
    if (pull.data) accept(pull.data);
  }, [pull.data, accept]);
  function refresh() {
    pull.retry();
    conversationLock?.refresh();
    setEpoch((value) => value + 1);
  }
  const data = current ?? pull.data;
  const publicReader = repo.public_read_only === true;
  const requestedTab = repositoryRoute(url)?.pullTab;
  const tab =
    requestedTab === "commits" ||
    requestedTab === "files" ||
    (requestedTab === "checks" && !publicReader)
      ? requestedTab
      : "conversation";
  const thread = Number(url.searchParams.get("thread"));
  const reply = Number(url.searchParams.get("reply"));
  const unresolvedOnly = url.searchParams.get("threads") === "unresolved";
  const linkedThread =
    !publicReader &&
    tab === "files" &&
    Number.isSafeInteger(thread) &&
    thread > 0;
  const pendingReview = usePendingReview(
    repo,
    number,
    csrf,
    !publicReader && tab === "files" && !linkedThread,
  );
  const [visitedConversation, setVisitedConversation] = useState(
    tab === "conversation",
  );
  useEffect(() => {
    if (tab === "conversation") setVisitedConversation(true);
  }, [tab]);
  const requestedCheck = Number(url.searchParams.get("check"));
  const selectedCheck =
    Number.isSafeInteger(requestedCheck) && requestedCheck > 0
      ? requestedCheck
      : undefined;
  const requestedCheckBefore = Number(url.searchParams.get("check_before"));
  const checkBefore =
    Number.isSafeInteger(requestedCheckBefore) && requestedCheckBefore > 0
      ? requestedCheckBefore
      : undefined;
  if (!data)
    return (
      <section className="pulls-page pull-detail">
        <Result state={pull}>{() => null}</Result>
      </section>
    );
  return (
    <section className="pulls-page pull-detail">
      <RequestWidget timing={pull.timing} label="Pull request">
        <>
          {pull.error && (
            <Alert variant="destructive">
              <AlertDescription>
                <p>{pull.error}</p>
                <Button variant="outline" onClick={refresh}>
                  Reload pull request
                </Button>
              </AlertDescription>
            </Alert>
          )}
          <div className="pull-title">
            <div className="flex flex-wrap items-start justify-between gap-3">
              <h2>
                {data.title} <span className="muted">#{data.number}</span>
              </h2>
              <EditPullRequest
                repo={repo}
                pull={data}
                csrf={csrf}
                accept={accept}
              />
            </div>
            <div className="pull-summary">
              <PullBadge pull={data} />
              {data.merge ? (
                <span>
                  <strong>
                    <Author author={data.merge.author} />
                  </strong>{" "}
                  {mergeMethod(data.merge.method).completion}{" "}
                  <code>{short(data.merge.commit_oid)}</code> on{" "}
                  <code>{branch(data.base_ref)}</code>
                </span>
              ) : (
                <span>
                  <strong>
                    <Author author={data.author} />
                  </strong>{" "}
                  wants to merge{" "}
                  {!data.head_ref ? (
                    "from an unavailable source branch"
                  ) : data.head_repository ? (
                    <>
                      <code>{short(data.head_oid)}</code> from{" "}
                      <Link
                        href={repoHref(data.head_repository, {
                          rev: data.head_ref,
                        })}
                      >
                        <code>
                          {data.head_repository.name}:{branch(data.head_ref)}
                        </code>
                      </Link>
                    </>
                  ) : (
                    <>
                      <code>{short(data.head_oid)}</code> from{" "}
                      <code>{branch(data.head_ref)}</code>
                    </>
                  )}{" "}
                  into <code>{branch(data.base_ref)}</code>
                </span>
              )}
              {data.can_change_base && !repo.archived && (
                <ChangeBase
                  repo={repo}
                  pull={data}
                  csrf={csrf}
                  theme={theme}
                  codeThemes={codeThemes}
                  refresh={refresh}
                />
              )}
            </div>
          </div>
          <ConversationLockControl
            repo={repo}
            target={`pulls/${number}`}
            csrf={csrf}
            onChanged={() => setEpoch((value) => value + 1)}
          />
          <nav className="pull-tabs" aria-label="Pull request">
            <Link
              className={tab === "conversation" ? "active" : ""}
              aria-current={tab === "conversation" ? "page" : undefined}
              href={repoHref(repo, {
                view: "pulls",
                pull: String(number),
              })}
            >
              <CommentIcon /> Conversation
            </Link>
            <Link
              className={tab === "commits" ? "active" : ""}
              aria-current={tab === "commits" ? "page" : undefined}
              href={repoHref(repo, {
                view: "pulls",
                pull: String(number),
                pull_tab: "commits",
              })}
            >
              <GitCommitIcon /> Commits
            </Link>
            {!publicReader && (
              <Link
                className={tab === "checks" ? "active" : ""}
                aria-current={tab === "checks" ? "page" : undefined}
                href={repoHref(repo, {
                  view: "pulls",
                  pull: String(number),
                  pull_tab: "checks",
                })}
              >
                <ChecklistIcon /> Checks
              </Link>
            )}
            <Link
              className={tab === "files" ? "active" : ""}
              aria-current={tab === "files" ? "page" : undefined}
              href={repoHref(repo, {
                view: "pulls",
                pull: String(number),
                pull_tab: "files",
              })}
            >
              Files changed
            </Link>
          </nav>
          {tab !== "checks" && !data.branches_available && (
            <div className="notice error">
              {publicReader
                ? "The source comparison is unavailable in this public view."
                : "One of this pull request’s branches no longer exists. The original commit IDs remain recorded, but the live comparison is unavailable."}
            </div>
          )}
          {tab === "checks" ? (
            <PullChecks
              key={`${repo.id}:${number}`}
              repo={repo}
              pull={data}
              csrf={csrf}
              selected={selectedCheck}
              before={checkBefore}
              showCandidate={url.searchParams.get("candidate") === "1"}
              onMerged={(merged) => {
                setCurrent(merged);
                refresh();
              }}
            />
          ) : tab === "commits" ? (
            data.branches_available ? (
              <PullCommits repo={repo} pull={data} url={url} />
            ) : null
          ) : linkedThread ? (
            <LinkedReviewThread
              key={`${thread}:${reply}`}
              repo={repo}
              pull={data}
              csrf={csrf}
              number={thread}
              refresh={refresh}
              reply={
                Number.isSafeInteger(reply) && reply > 0 ? reply : undefined
              }
            />
          ) : tab === "files" && publicReader ? (
            data.branches_available && (
              <PublicPullFiles
                repo={repo}
                pull={data}
                theme={theme}
                codeThemes={codeThemes}
              />
            )
          ) : tab === "files" ? (
            <>
              {data.branches_available && (
                <PullCodeOwners repo={repo} pull={data} />
              )}
              {(reviewDraft?.attempt ||
                (data.branches_available &&
                  data.state === "open" &&
                  !repo.archived)) && (
                <ReviewChanges
                  repo={repo}
                  pull={data}
                  csrf={csrf}
                  refresh={refresh}
                  disabled={
                    pull.loading ||
                    Boolean(pull.error) ||
                    conversationLock?.lock?.can_post === false
                  }
                  draft={reviewDraft}
                  onDraft={setReviewDraft}
                  submission={reviewSubmission}
                  pendingReview={pendingReview}
                />
              )}
              <PendingComments
                review={pendingReview}
                head={data.head_oid}
                canEdit={data.branches_available && data.state === "open"}
                canDiscard={!repo.archived}
                busy={!!reviewDraft?.attempt}
              />
              {data.branches_available && (
                <PullReviewThreads
                  key={unresolvedOnly ? "unresolved" : "all"}
                  unresolvedOnly={unresolvedOnly}
                  repo={repo}
                  pull={data}
                  csrf={csrf}
                  theme={theme}
                  codeThemes={codeThemes}
                  refresh={refresh}
                  pendingReview={pendingReview}
                  submittingReview={!!reviewDraft?.attempt}
                />
              )}
            </>
          ) : null}
          {/* Keep drafts and pending writes mounted; hide portals with their panel. */}
          {(tab === "conversation" || visitedConversation) && (
            <OverlayVisibility value={tab === "conversation"}>
              <div hidden={tab !== "conversation"}>
                <div className="discussion-detail-layout">
                  <PullConversation
                    comment={comment}
                    active={tab === "conversation"}
                    refreshing={pull.loading || Boolean(pull.error)}
                    revision={`${data.version}:${epoch}`}
                    repo={repo}
                    pull={data}
                    csrf={csrf}
                    refresh={refresh}
                  />
                  <DiscussionMetadata
                    repo={repo}
                    assignees={data.assignees}
                    labels={data.labels}
                    canAssign={data.can_assign && !repo.archived}
                    canLabel={data.can_label && !repo.archived}
                    version={data.version}
                    path={`pulls/${number}`}
                    csrf={csrf}
                    onSaved={refresh}
                    allowIssueClosure={data.state !== "merged"}
                    reviewerRefresh={epoch}
                  />
                </div>
              </div>
            </OverlayVisibility>
          )}
        </>
      </RequestWidget>
    </section>
  );
}
