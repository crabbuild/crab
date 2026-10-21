import {
  DiscussionListControls,
  DiscussionCommentCount,
  discussionFilters,
  type DiscussionPage,
} from "@/components/discussion/list";
import { repoHref } from "@/lib/repository-route";

import { Button } from "@/components/ui/button";

import { IssueClosedIcon, IssueOpenedIcon } from "@primer/octicons-react";
import { endpoint, navigate, useRequest, type Repository } from "@/lib/api";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";

import { DiscussionSearch } from "@/components/discussion/search";
import { LabelBadges } from "@/features/labels/selection";
import { AssigneeAvatars } from "@/components/discussion/metadata";

import type { IssueListItem } from "./model";
import { timestamp } from "@/components/discussion/time";

export function IssueList({ repo, url }: { repo: Repository; url: URL }) {
  const filters = discussionFilters(url);
  const before = url.searchParams.get("before") ?? undefined;
  const query = url.searchParams.get("q") ?? "";
  const page = useRequest<DiscussionPage<IssueListItem>>(
    endpoint(repo, "issues", { ...filters, before }),
  );
  return (
    <section className="issues-page">
      <div className="issues-heading">
        <h2>All issues</h2>
        {!repo.archived && (
          <Button
            variant="default"
            onClick={() =>
              navigate(repoHref(repo, { view: "issues", issue: "new" }))
            }
          >
            New issue
          </Button>
        )}
      </div>
      <DiscussionSearch
        repo={repo}
        kind="issue"
        label="Search issues"
        placeholder="Search all issues"
        value={query}
        onSearch={(value) =>
          navigate(
            repoHref(repo, {
              view: "issues",
              ...filters,
              q: value || undefined,
            }),
          )
        }
      />
      {before && (
        <Link
          className="discussion-page-reset"
          href={repoHref(repo, { view: "issues", ...filters })}
        >
          Back to first page
        </Link>
      )}
      <div className="issue-list-panel panel">
        <Result state={page}>
          {(data) => (
            <>
              <DiscussionListControls
                repo={repo}
                url={url}
                view="issues"
                counts={data.counts}
                navigation={data.navigation}
                matching={data.matching}
              />
              {data.items.length ? (
                <ul className="issue-list">
                  {data.items.map((issue) => (
                    <li key={issue.number}>
                      <span
                        className={`issue-status-icon ${issue.state}`}
                        role="img"
                        aria-label={issue.state}
                      >
                        {issue.state === "open" ? (
                          <IssueOpenedIcon />
                        ) : (
                          <IssueClosedIcon />
                        )}
                      </span>
                      <div>
                        <Link
                          className="issue-link"
                          href={repoHref(repo, {
                            view: "issues",
                            issue: String(issue.number),
                          })}
                        >
                          {issue.title}
                        </Link>
                        <LabelBadges labels={issue.labels} />
                        <p className="muted">
                          #{issue.number} opened {timestamp(issue.created_at)}{" "}
                          by {issue.author}
                        </p>
                      </div>
                      <AssigneeAvatars assignees={issue.assignees} />
                      <DiscussionCommentCount
                        count={issue.comment_count}
                        href={repoHref(repo, {
                          view: "issues",
                          issue: String(issue.number),
                        })}
                      />
                    </li>
                  ))}
                </ul>
              ) : (
                <div className="issue-empty">
                  <IssueOpenedIcon size={32} />
                  <h3>{query ? `No issues match “${query}”` : "No results"}</h3>
                  <p className="muted">
                    {query
                      ? "Try adjusting your search or filters."
                      : "Try adjusting your search filters."}
                  </p>
                </div>
              )}
              <div className="discussion-pagination">
                {data.next && (
                  <Link
                    href={repoHref(repo, {
                      view: "issues",
                      ...filters,
                      q: query || undefined,
                      before: data.next,
                    })}
                  >
                    Older issues →
                  </Link>
                )}
              </div>
            </>
          )}
        </Result>
      </div>
    </section>
  );
}
