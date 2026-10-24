import { useBrowserPreference } from "@/lib/browser-preference";
import { CircleDotIcon, GitPullRequestIcon } from "lucide-react";
import {
  Empty,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
  EmptyDescription,
} from "@/components/ui/empty";
import { Link } from "@/components/navigation/link";
import { Result, RequestWidget } from "@/components/request/result";
import { endpoint, navigate, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import {
  DiscussionListControls,
  discussionFilters,
  discussionQuery,
  type DiscussionPage,
} from "./list";
import { useDiscussionNavigation } from "./workspace";
import { DiscussionRow, type DiscussionItem } from "./row";
import { BulkActions, BulkCheckbox, useBulkEdit } from "./bulk";

export function DiscussionResults({
  repo,
  url,
  view,
  csrf,
}: {
  repo: Repository;
  url: URL;
  view: "issues" | "pulls";
  csrf: string;
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
  const bulk = useBulkEdit(
    repo,
    csrf,
    page.data?.items ?? [],
    page.data?.repository_id,
    url.pathname + url.search,
    () => {
      if (before) navigate(repoHref(repo, { view, ...filters }));
      else page.retry();
    },
  );
  const selectable = Boolean(page.data?.can_bulk_edit && !repo.archived);
  const selection =
    selectable || bulk.selected.length ? (
      <BulkCheckbox
        bulk={bulk}
        count={page.data?.items.length ?? 0}
        disabled={!selectable || page.loading}
        loading={page.loading}
      />
    ) : undefined;
  useDiscussionNavigation(
    view,
    discussionQuery(url, view),
    page.data?.authored_by_me,
    page.data?.assigned_to_me,
    page.data?.review_requested_from_me,
  );
  const Icon = view === "issues" ? CircleDotIcon : GitPullRequestIcon;
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
          <BulkActions repo={repo} bulk={bulk} selection={selection} />
          {!bulk.selected.length && (
            <DiscussionListControls
              repo={repo}
              url={url}
              view={view}
              data={page.data}
              loading={page.loading}
              density={density}
              onDensityChange={setDensity}
              selection={selection}
            />
          )}
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
                        selection={
                          selectable || bulk.selected.length
                            ? {
                                checked: bulk.checked(item),
                                disabled:
                                  !selectable ||
                                  bulk.locked ||
                                  bulk.identityChanged,
                                toggle: (range) => bulk.toggle(item, range),
                              }
                            : undefined
                        }
                      />
                    ))}
                  </ul>
                ) : (
                  <Empty className="min-h-64 py-12">
                    <EmptyHeader>
                      <EmptyMedia variant="icon">
                        <Icon aria-hidden="true" />
                      </EmptyMedia>
                      <EmptyTitle role="heading" aria-level={3}>
                        {view === "issues"
                          ? query
                            ? `No results match “${query}”`
                            : "No results"
                          : query
                            ? `No pulls match “${query}”`
                            : "No matching pulls"}
                      </EmptyTitle>
                      <EmptyDescription>
                        {query
                          ? "Try adjusting your search or filters."
                          : view === "issues"
                            ? "Try adjusting your search filters."
                            : "Compare two branches to start a review."}
                      </EmptyDescription>
                    </EmptyHeader>
                  </Empty>
                )}
                {data.next && (
                  <div className="discussion-pagination">
                    <Link
                      href={repoHref(repo, {
                        view,
                        ...filters,
                        before: data.next,
                      })}
                    >
                      {view === "issues" ? "Older results →" : "Older pulls →"}
                    </Link>
                  </div>
                )}
              </>
            )}
          </Result>
        </div>
      </RequestWidget>
    </>
  );
}
