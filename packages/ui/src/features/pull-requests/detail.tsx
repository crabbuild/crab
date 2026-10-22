import { Author } from "@/components/discussion/author";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { CheckRuns } from "./checks";
import type { CodeThemes } from "@/lib/code-theme";
import { DiscussionMetadata } from "@/components/discussion/metadata";
import { repoHref, repositoryRoute } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { short } from "@/lib/format";
import {
  ChecklistIcon,
  CommentIcon,
  GitCommitIcon,
} from "@primer/octicons-react";

import { PullCommits } from "./commits";
import { PullConversation } from "./conversation";
import type { PullRequest } from "./model";
import { branch, pullIcons, pullStatus } from "./presentation";
import { ReviewForm } from "./reviews/form";
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
  const path = endpoint(repo, `pulls/${number}`);
  const pull = useRequest<PullRequest>(path);
  const requestedTab = repositoryRoute(url)?.pullTab;
  const tab =
    requestedTab === "commits" ||
    requestedTab === "files" ||
    requestedTab === "checks"
      ? requestedTab
      : "conversation";
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
  return (
    <section className="pulls-page pull-detail">
      <Result state={pull} timingLabel="Pull request">
        {(data) => (
          <>
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
                  <PullReviewThreads
                    repo={repo}
                    pull={data}
                    csrf={csrf}
                    theme={theme}
                    codeThemes={codeThemes}
                    refresh={pull.retry}
                  />
                  {data.state === "open" && !repo.archived && (
                    <ReviewForm
                      repo={repo}
                      pull={data}
                      csrf={csrf}
                      refresh={pull.retry}
                    />
                  )}
                </>
              ) : null
            ) : (
              <div className="discussion-detail-layout">
                <PullConversation
                  repo={repo}
                  pull={data}
                  csrf={csrf}
                  refresh={pull.retry}
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
                  onSaved={pull.retry}
                />
              </div>
            )}
          </>
        )}
      </Result>
    </section>
  );
}
