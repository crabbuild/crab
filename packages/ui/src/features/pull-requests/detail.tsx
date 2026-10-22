import { useEffect, useState } from "react";
import { OverlayVisibility } from "@/components/ui/use-overlay-state";
import { useSubmission } from "@/components/discussion/hooks";
import { Author } from "@/components/discussion/author";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { CheckRuns } from "./checks";
import type { CodeThemes } from "@/lib/code-theme";
import { DiscussionMetadata } from "@/components/discussion/metadata";
import { repoHref, repositoryRoute } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";
import { Result, RequestWidget } from "@/components/request/result";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { short } from "@/lib/format";
import {
  ChecklistIcon,
  CommentIcon,
  GitCommitIcon,
} from "@primer/octicons-react";

import { PullCommits } from "./commits";
import { PullConversation } from "./conversation";
import { useCommentSubmission } from "@/components/discussion/conversation/compose";
import type { PullRequest } from "./model";
import { branch, pullIcons, pullStatus } from "./presentation";
import { ReviewChanges, type ReviewDraft } from "./reviews/form";
import { PullReviewThreads } from "./reviews/threads";

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
  const [reviewDraft, setReviewDraft] = useState<ReviewDraft>();
  const reviewSubmission = useSubmission();
  // Draft and submission identity outlive tab changes and metadata refreshes.
  const comment = useCommentSubmission(repo, `pulls/${number}`, csrf);
  const path = endpoint(repo, `pulls/${number}`);
  const pull = useRequest<PullRequest>(path);
  const [current, setCurrent] = useState<PullRequest>();
  const [epoch, setEpoch] = useState(0);
  useEffect(() => {
    const incoming = pull.data;
    if (incoming)
      setCurrent((old) =>
        old && old.version > incoming.version ? old : incoming,
      );
  }, [pull.data]);
  function refresh() {
    pull.retry();
    setEpoch((value) => value + 1);
  }
  const data = current ?? pull.data;
  const requestedTab = repositoryRoute(url)?.pullTab;
  const tab =
    requestedTab === "commits" ||
    requestedTab === "files" ||
    requestedTab === "checks"
      ? requestedTab
      : "conversation";
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
            <h2>
              {data.title} <span className="muted">#{data.number}</span>
            </h2>
            <div className="pull-summary">
              <PullBadge pull={data} />
              {data.merge ? (
                <span>
                  <strong>
                    <Author author={data.merge.author} />
                  </strong>{" "}
                  {data.merge.method === "merge_commit"
                    ? "created merge commit"
                    : "fast-forwarded"}{" "}
                  <code>{short(data.merge.commit_oid)}</code> into{" "}
                  <code>{branch(data.base_ref)}</code>
                </span>
              ) : (
                <span>
                  <strong>
                    <Author author={data.author} />
                  </strong>{" "}
                  wants to merge <code>{short(data.head_oid)}</code> from{" "}
                  <code>{branch(data.head_ref)}</code> into{" "}
                  <code>{branch(data.base_ref)}</code>
                </span>
              )}
            </div>
          </div>
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
              One of this pull request&apos;s branches no longer exists. The
              original commit IDs remain recorded, but the live comparison is
              unavailable.
            </div>
          )}
          {tab === "checks" ? (
            <CheckRuns
              repo={repo}
              pull={number}
              oid={data.head_oid}
              selected={selectedCheck}
              before={checkBefore}
            />
          ) : tab === "commits" ? (
            data.branches_available ? (
              <PullCommits repo={repo} pull={data} url={url} />
            ) : null
          ) : tab === "files" ? (
            data.branches_available ? (
              <>
                {data.state === "open" && !repo.archived && (
                  <ReviewChanges
                    repo={repo}
                    pull={data}
                    csrf={csrf}
                    refresh={refresh}
                    disabled={pull.loading || Boolean(pull.error)}
                    draft={reviewDraft}
                    onDraft={setReviewDraft}
                    submission={reviewSubmission}
                  />
                )}
                <PullReviewThreads
                  repo={repo}
                  pull={data}
                  csrf={csrf}
                  theme={theme}
                  codeThemes={codeThemes}
                  refresh={refresh}
                />
              </>
            ) : null
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
