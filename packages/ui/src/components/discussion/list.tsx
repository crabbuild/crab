import { ChevronDownIcon, MessageSquareIcon } from "lucide-react";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { navigate, type Repository, type RepositoryLabel } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";

export type ListLabel = Pick<RepositoryLabel, "id" | "name" | "color">;
export interface DiscussionPage<T> {
  items: T[];
  next: string | null;
  counts: { open: number; closed: number; merged: number };
  matching: number;
}
export interface DiscussionTotals {
  comment_count: number;
  reaction_count: number;
}
const sorts = [
  ["created", "Created"],
  ["updated", "Updated"],
  ["comments", "Comments"],
  ["reactions", "Total reactions"],
  ["thumbs_up", "👍 reactions"],
  ["thumbs_down", "👎 reactions"],
  ["laugh", "😄 reactions"],
  ["hooray", "🎉 reactions"],
  ["confused", "😕 reactions"],
  ["heart", "❤️ reactions"],
  ["rocket", "🚀 reactions"],
  ["eyes", "👀 reactions"],
  ["relevance", "Relevance"],
] as const;

// Search, state, sort, and pagination share one URL. A changed filter starts at
// the first page because the native cursor belongs to its original query.
export function discussionFilters(url: URL) {
  return {
    state: url.searchParams.get("state") ?? "open",
    q: url.searchParams.get("q") || undefined,
    sort: url.searchParams.get("sort") ?? "created",
    direction: url.searchParams.get("direction") ?? "desc",
  };
}

export function DiscussionListControls({
  repo,
  url,
  view,
  counts,
}: {
  repo: Repository;
  url: URL;
  view: "issues" | "pulls";
  counts: DiscussionPage<unknown>["counts"];
}) {
  const filters = discussionFilters(url);
  const sortLabel = sorts.find(([key]) => key === filters.sort)?.[1];
  const change = (values: Partial<typeof filters>) =>
    navigate(repoHref(repo, { view, ...filters, ...values }));
  return (
    <div className="issues-filters">
      <nav
        aria-label={view === "issues" ? "Issue state" : "Pull request state"}
      >
        {(["open", "closed", "all"] as const).map((state) => (
          <Link
            key={state}
            className={filters.state === state ? "active" : ""}
            aria-current={filters.state === state ? "page" : undefined}
            href={repoHref(repo, { view, ...filters, state })}
          >
            {state === "all" ? "All" : state === "open" ? "Open" : "Closed"}{" "}
            <span className="discussion-state-count">
              {new Intl.NumberFormat().format(
                state === "all" ? counts.open + counts.closed : counts[state],
              )}
            </span>
          </Link>
        ))}
      </nav>
      <DropdownMenu modal={false}>
        <DropdownMenuTrigger asChild>
          <Button variant="ghost" size="sm" aria-label="Sort discussions">
            Sort <ChevronDownIcon size={14} />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-56" tabIndex={0}>
          <DropdownMenuLabel>Sort by</DropdownMenuLabel>
          <DropdownMenuRadioGroup
            value={filters.sort}
            onValueChange={(sort) => change({ sort })}
          >
            {sorts.map(([value, label]) => (
              <DropdownMenuRadioItem key={value} value={value}>
                {label}
              </DropdownMenuRadioItem>
            ))}
          </DropdownMenuRadioGroup>
          <DropdownMenuSeparator />
          <DropdownMenuLabel>Order</DropdownMenuLabel>
          <DropdownMenuRadioGroup
            value={filters.direction}
            onValueChange={(direction) => change({ direction })}
          >
            <DropdownMenuRadioItem value="desc">
              {filters.sort === "created" || filters.sort === "updated"
                ? "Newest first"
                : "Highest first"}
            </DropdownMenuRadioItem>
            <DropdownMenuRadioItem value="asc">
              {filters.sort === "created" || filters.sort === "updated"
                ? "Oldest first"
                : "Lowest first"}
            </DropdownMenuRadioItem>
          </DropdownMenuRadioGroup>
        </DropdownMenuContent>
      </DropdownMenu>
      <span className="sr-only" aria-live="polite">
        Sorted by {sortLabel},{" "}
        {filters.direction === "asc" ? "ascending" : "descending"}
      </span>
    </div>
  );
}

export function DiscussionCommentCount({
  count,
  href,
}: {
  count: number;
  href: string;
}) {
  if (!count) return null;
  return (
    <Link
      className="discussion-comment-count"
      href={href}
      aria-label={`${count} ${count === 1 ? "comment" : "comments"}`}
    >
      <MessageSquareIcon size={16} /> {count}
    </Link>
  );
}
