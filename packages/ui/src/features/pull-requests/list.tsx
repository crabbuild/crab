import {
  DiscussionListControls,
  discussionFilters,
  type DiscussionPage,
} from "@/components/discussion/list";
import { endpoint, navigate, useRequest, type Repository } from "@/lib/api";
import { DiscussionSearch } from "@/components/discussion/search";
import { Button } from "@/components/ui/button";
import { repoHref } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { GitPullRequestIcon } from "@primer/octicons-react";

import type { PullListItem } from "./model";
import { DiscussionRow } from "@/components/discussion/row";

export function PullList({ repo, url }: { repo: Repository; url: URL }) {
  const filters = discussionFilters(url);
  const before = url.searchParams.get("before") ?? undefined;
  const query = url.searchParams.get("q") ?? "";
  const page = useRequest<DiscussionPage<PullListItem>>(
    endpoint(repo, "pulls", { ...filters, before }),
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
        label="Search pulls"
        placeholder="Search titles, descriptions, or authors"
        value={query}
        onSearch={(value) =>
          navigate(
            repoHref(repo, {
              view: "pulls",
              ...filters,
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
      {before && (
        <Link
          className="discussion-page-reset"
          href={repoHref(repo, { view: "pulls", ...filters })}
        >
          Back to first page
        </Link>
      )}
      <Result state={page} timingLabel="Pull requests">
        {(data) => (
          <>
            <DiscussionListControls
              repo={repo}
              url={url}
              view="pulls"
              counts={data.counts}
              navigation={data.navigation}
              matching={data.matching}
            />
            {data.items.length ? (
              <ul className="issue-list panel">
                {data.items.map((item) => (
                  <DiscussionRow
                    key={`${item.kind}:${item.number}`}
                    item={item}
                    repo={repo}
                  />
                ))}
              </ul>
            ) : (
              <div className="notice issue-empty">
                <GitPullRequestIcon size={32} />
                <h3>
                  {query ? `No pulls match “${query}”` : "No matching pulls"}
                </h3>
                <p>
                  {query
                    ? "Try adjusting your search or filters."
                    : "Compare two branches to start a review."}
                </p>
              </div>
            )}
            <div className="discussion-pagination">
              {data.next && (
                <Link
                  href={repoHref(repo, {
                    view: "pulls",
                    ...filters,
                    q: query || undefined,
                    before: data.next,
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
