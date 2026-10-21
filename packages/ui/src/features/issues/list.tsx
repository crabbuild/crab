import { repoHref } from "@/repository-route";

import { Button } from "@/components/ui/button";

import { IssueClosedIcon, IssueOpenedIcon } from "@primer/octicons-react";
import { endpoint, navigate, useRequest, type Repository } from "@/api";
import { Link, Result } from "@/ui";

import { DiscussionSearch } from "@/components/discussion/search";
import { LabelBadges } from "@/discussion-labels";
import { AssigneeAvatars } from "@/components/discussion/metadata";

import type { IssueListItem, Page } from "./model";
import { timestamp } from "@/components/discussion/time";
export function IssueList({ repo, url }: { repo: Repository; url: URL }) {
  const state = url.searchParams.get("state") ?? "open";
  const before = url.searchParams.get("before") ?? undefined;
  const query = url.searchParams.get("q") ?? "";
  const page = useRequest<Page<IssueListItem>>(
    endpoint(repo, "issues", { state, before, q: query || undefined }),
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
        label="Search issues"
        placeholder="Search all issues"
        value={query}
        onSearch={(value) =>
          navigate(
            repoHref(repo, {
              view: "issues",
              state,
              q: value || undefined,
            }),
          )
        }
      />
      <div className="issue-list-panel panel">
        <div className="issues-filters">
          <nav aria-label="Issue state">
            {["open", "closed", "all"].map((value) => (
              <Link
                key={value}
                className={state === value ? "active" : ""}
                aria-current={state === value ? "page" : undefined}
                href={repoHref(repo, {
                  view: "issues",
                  state: value,
                  q: query || undefined,
                })}
              >
                {value === "all"
                  ? "All"
                  : `${value[0].toUpperCase()}${value.slice(1)}`}
              </Link>
            ))}
          </nav>
        </div>
        <Result state={page}>
          {(data) => (
            <>
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
                    </li>
                  ))}
                </ul>
              ) : (
                <div className="issue-empty">
                  <IssueOpenedIcon size={32} />
                  <h3>
                    {query
                      ? `No issues match “${query}”`
                      : data.next
                        ? "No matching issues in this range"
                        : "No results"}
                  </h3>
                  <p className="muted">
                    {query
                      ? data.next
                        ? "Try another search or continue to older issues."
                        : "Try another title, description, or author."
                      : data.next
                        ? "Continue to older issues or choose another filter."
                        : "Try adjusting your search filters."}
                  </p>
                </div>
              )}
              <div className="discussion-pagination">
                {before && (
                  <Link
                    href={repoHref(repo, {
                      view: "issues",
                      state,
                      q: query || undefined,
                    })}
                  >
                    Newest issues
                  </Link>
                )}
                {data.next && (
                  <Link
                    href={repoHref(repo, {
                      view: "issues",
                      state,
                      q: query || undefined,
                      before: String(data.next),
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
