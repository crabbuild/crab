import { useBrowserPreference } from "@/lib/browser-preference";
import { IssueOpenedIcon, GitPullRequestIcon } from "@primer/octicons-react";
import { Link } from "@/components/navigation/link";
import { Result, RequestWidget } from "@/components/request/result";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import {
  DiscussionListControls,
  discussionFilters,
  discussionQuery,
  type DiscussionPage,
} from "./list";
import { useDiscussionNavigation } from "./workspace";
import { DiscussionRow, type DiscussionItem } from "./row";

export function DiscussionResults({
  repo,
  url,
  view,
}: {
  repo: Repository;
  url: URL;
  view: "issues" | "pulls";
}) {
  const [density, setDensity] = useBrowserPreference(
    "crab-discussion-density",
    ["comfortable", "compact"] as const,
  );
  const filters = discussionFilters(url);
  const before = url.searchParams.get("before") ?? undefined;
  const query = url.searchParams.get("q") ?? "";
  const page = useRequest<DiscussionPage<DiscussionItem>>(
    endpoint(repo, view, { ...filters, before }),
  );
  useDiscussionNavigation(
    view,
    discussionQuery(url, view),
    page.data?.authored_by_me,
    page.data?.assigned_to_me,
    page.data?.review_requested_from_me,
  );
  const Icon = view === "issues" ? IssueOpenedIcon : GitPullRequestIcon;
  return (
    <>
      {before && (
        <Link
          className="discussion-page-reset"
          href={repoHref(repo, { view, ...filters })}
        >
          Back to first page
        </Link>
      )}
      <RequestWidget
        timing={page.timing}
        label={view === "issues" ? "Issues" : "Pull requests"}
      >
        <div className="issue-list-panel panel">
          {/* Filters retain their open interaction across refreshes. Counts and
            rows clear while loading; one timing boundary owns the whole list. */}
          <DiscussionListControls
            repo={repo}
            url={url}
            view={view}
            data={page.data}
            loading={page.loading}
            density={density}
            onDensityChange={setDensity}
          />
          <Result state={{ ...page, timing: undefined }}>
            {(data) => (
              <>
                {data.items.length ? (
                  <ul className="issue-list" data-density={density}>
                    {data.items.map((item) => (
                      <DiscussionRow
                        key={`${item.kind}:${item.number}`}
                        item={item}
                        repo={repo}
                      />
                    ))}
                  </ul>
                ) : (
                  <div className="issue-empty">
                    <Icon size={32} />
                    <h3>
                      {view === "issues"
                        ? query
                          ? `No results match “${query}”`
                          : "No results"
                        : query
                          ? `No pulls match “${query}”`
                          : "No matching pulls"}
                    </h3>
                    <p className="muted">
                      {query
                        ? "Try adjusting your search or filters."
                        : view === "issues"
                          ? "Try adjusting your search filters."
                          : "Compare two branches to start a review."}
                    </p>
                  </div>
                )}
                <div className="discussion-pagination">
                  {data.next && (
                    <Link
                      href={repoHref(repo, {
                        view,
                        ...filters,
                        before: data.next,
                      })}
                    >
                      {view === "issues" ? "Older results →" : "Older pulls →"}
                    </Link>
                  )}
                </div>
              </>
            )}
          </Result>
        </div>
      </RequestWidget>
    </>
  );
}
