import { endpoint, navigate, useRequest, type Repository } from "@/lib/api";
import { AssigneeAvatars } from "@/components/discussion/metadata";
import { DiscussionSearch } from "@/components/discussion/search";
import { Button } from "@/components/ui/button";
import { LabelBadges } from "@/features/labels/selection";
import { repoHref } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { GitPullRequestIcon } from "@primer/octicons-react";

import { timestamp } from "@/components/discussion/time";
import type { Page, PullSummary } from "./model";
import { branch, pullIcons, pullStatus } from "./presentation";

export function PullList({ repo, url }: { repo: Repository; url: URL }) {
  const state = url.searchParams.get("state") ?? "open";
  const before = url.searchParams.get("before") ?? undefined;
  const query = url.searchParams.get("q") ?? "";
  const page = useRequest<Page<PullSummary>>(
    endpoint(repo, "pulls", { state, before, q: query || undefined }),
  );
  return (
    <section className="pulls-page">
      <div className="section-heading">
        <h2>Pull requests</h2>
        {!repo.archived && (
          <Button
            variant="default"
            onClick={() =>
              navigate(repoHref(repo, { view: "pulls", pull: "new" }))
            }
          >
            New pull request
          </Button>
        )}
      </div>
      <p className="muted">Review and discuss changes between branches.</p>
      <DiscussionSearch
        repo={repo}
        kind="pull"
        label="Search pulls"
        placeholder="Search titles, descriptions, or authors"
        value={query}
        onSearch={(value) =>
          navigate(
            repoHref(repo, {
              view: "pulls",
              state,
              q: value || undefined,
            }),
          )
        }
      />
      <div className="discussion-list-actions">
        <Link className="button-link" href={repoHref(repo, { view: "labels" })}>
          Labels
        </Link>
      </div>
      <div className="issues-filters">
        <nav aria-label="Pull request state">
          {["open", "closed", "all"].map((value) => (
            <Link
              key={value}
              className={state === value ? "active" : ""}
              aria-current={state === value ? "page" : undefined}
              href={repoHref(repo, {
                view: "pulls",
                state: value,
                q: query || undefined,
              })}
            >
              {value === "all"
                ? "All pulls"
                : `${value[0].toUpperCase()}${value.slice(1)}`}
            </Link>
          ))}
        </nav>
        <Button variant="outline" size="sm" onClick={page.retry}>
          Refresh
        </Button>
      </div>
      <Result state={page} timingLabel="Pull requests">
        {(data) => (
          <>
            {data.items.length ? (
              <ul className="issue-list panel">
                {data.items.map((pull) => {
                  const status = pullStatus(pull);
                  const Icon = pullIcons[status];
                  return (
                    <li key={pull.number}>
                      <span
                        className={`pull-status-icon ${status}`}
                        role="img"
                        aria-label={status}
                      >
                        <Icon />
                      </span>
                      <div>
                        <Link
                          className="issue-link"
                          href={repoHref(repo, {
                            view: "pulls",
                            pull: String(pull.number),
                          })}
                        >
                          {pull.title}
                        </Link>
                        <LabelBadges labels={pull.labels} />
                        <p className="muted">
                          #{pull.number} opened {timestamp(pull.created_at)} by{" "}
                          {pull.author}
                        </p>
                        <p className="pull-branches muted">
                          <code>{branch(pull.head_ref)}</code>
                          <span>into</span>
                          <code>{branch(pull.base_ref)}</code>
                        </p>
                      </div>
                      <AssigneeAvatars assignees={pull.assignees} />
                    </li>
                  );
                })}
              </ul>
            ) : (
              <div className="notice issue-empty">
                <GitPullRequestIcon size={32} />
                <h3>
                  {query ? `No pulls match “${query}”` : "No matching pulls"}
                </h3>
                <p>
                  {query
                    ? data.next
                      ? "Try another search or continue to older pulls."
                      : "Try another title, description, or author."
                    : "Compare two branches to start a review."}
                </p>
              </div>
            )}
            <div className="discussion-pagination">
              {before && (
                <Link
                  href={repoHref(repo, {
                    view: "pulls",
                    state,
                    q: query || undefined,
                  })}
                >
                  Newest pulls
                </Link>
              )}
              {data.next && (
                <Link
                  href={repoHref(repo, {
                    view: "pulls",
                    state,
                    q: query || undefined,
                    before: String(data.next),
                  })}
                >
                  Older pulls →
                </Link>
              )}
            </div>
          </>
        )}
      </Result>
    </section>
  );
}
