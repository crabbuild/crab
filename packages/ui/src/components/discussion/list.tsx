import { useState } from "react";
import {
  AlignJustifyIcon,
  Rows2Icon,
  ChevronDownIcon,
  MessageSquareIcon,
} from "lucide-react";
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
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { DiscussionFilter } from "./filter";

export type ListLabel = Pick<RepositoryLabel, "id" | "name" | "color">;
export interface DiscussionPage<T> {
  items: T[];
  next: string | null;
  counts: { open: number; closed: number; merged: number };
  matching: number;
  authored_by_me: boolean;
  navigation:
    | { kind: "results" }
    | {
        kind: "state";
        selected: "open" | "closed";
        open: string | null;
        closed: string | null;
      };
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
export function discussionQuery(url: URL, view: "issues" | "pulls") {
  return (
    url.searchParams.get("q") ||
    (view === "issues" ? "is:issue is:open" : "is:open")
  );
}

export function discussionFilters(url: URL) {
  return {
    q: url.searchParams.get("q") || undefined,
    sort: url.searchParams.get("sort") ?? "created",
    direction: url.searchParams.get("direction") ?? "desc",
  };
}

export function DiscussionListControls({
  repo,
  url,
  view,
  data,
  loading,
  density,
  onDensityChange,
}: {
  repo: Repository;
  url: URL;
  view: "issues" | "pulls";
  data?: Pick<DiscussionPage<unknown>, "counts" | "navigation" | "matching">;
  loading: boolean;
  density: "comfortable" | "compact";
  onDensityChange: (density: "comfortable" | "compact") => void;
}) {
  const filters = discussionFilters(url);
  const [sortOpen, setSortOpen] = useState(false);
  const navigation = data?.navigation;
  const cannotChangeState =
    navigation?.kind === "state" &&
    (navigation.open === null || navigation.closed === null);
  const sortLabel = sorts.find(([key]) => key === filters.sort)?.[1];
  const change = (values: Partial<typeof filters>) =>
    navigate(repoHref(repo, { view, ...filters, ...values }));
  return (
    <div className="issues-filters flex-wrap">
      <div className="discussion-counts">
        {!data ? (
          <span role="status" className="text-sm text-muted-foreground">
            {loading ? "Loading results…" : "Results unavailable"}
          </span>
        ) : navigation?.kind === "state" ? (
          <nav
            aria-label={
              view === "issues" ? "Issue state" : "Pull request state"
            }
          >
            {(["open", "closed"] as const).map((state) => {
              const query = navigation[state];
              const label = (
                <>
                  {state === "open" ? "Open" : "Closed"}{" "}
                  <span className="discussion-state-count">
                    {new Intl.NumberFormat().format(data.counts[state])}
                  </span>
                </>
              );
              return query === null ? (
                <Button
                  key={state}
                  variant="ghost"
                  size="sm"
                  disabled
                  title="Shorten this search to change its state filter"
                >
                  {label}
                </Button>
              ) : (
                <Link
                  key={state}
                  className={navigation.selected === state ? "active" : ""}
                  aria-current={
                    navigation.selected === state ? "page" : undefined
                  }
                  href={repoHref(repo, { view, ...filters, q: query })}
                >
                  {label}
                </Link>
              );
            })}
          </nav>
        ) : (
          <h3 className="m-0 text-sm font-medium">
            {new Intl.NumberFormat().format(data.matching)}{" "}
            {data.matching === 1 ? "result" : "results"}
          </h3>
        )}
      </div>
      <div className="ml-auto flex min-w-0 max-w-full flex-wrap items-center justify-end gap-1">
        {(["authors", "labels", "milestones"] as const).map((kind) => (
          <DiscussionFilter
            key={kind}
            kind={kind}
            repo={repo}
            query={discussionQuery(url, view)}
            onSelect={(q) => change({ q })}
          />
        ))}
        <DropdownMenu modal={false} open={sortOpen} onOpenChange={setSortOpen}>
          <DropdownMenuTrigger asChild>
            <Button variant="ghost" size="sm" aria-label="Sort discussions">
              Sort <ChevronDownIcon size={14} />
            </Button>
          </DropdownMenuTrigger>
          {/* Closing scopes must unmount before a rapid reopen during refresh. */}
          {sortOpen && (
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
          )}
        </DropdownMenu>
        <ToggleGroup
          className="discussion-density"
          type="single"
          variant="outline"
          size="sm"
          spacing={0}
          aria-label="List density"
          value={density}
          onValueChange={(value) => {
            if (value === "comfortable" || value === "compact")
              onDensityChange(value);
          }}
        >
          {(
            [
              { value: "comfortable", label: "Comfortable", Icon: Rows2Icon },
              { value: "compact", label: "Compact", Icon: AlignJustifyIcon },
            ] as const
          ).map(({ value, label, Icon }) => (
            <Tooltip key={value}>
              <TooltipTrigger asChild>
                {/* Tooltip and toggle each own data-state; keep the tooltip
                      on its wrapper so it cannot replace the selected style. */}
                <span className="inline-flex">
                  <ToggleGroupItem value={value} aria-label={label}>
                    <Icon />
                  </ToggleGroupItem>
                </span>
              </TooltipTrigger>
              <TooltipContent>{label}</TooltipContent>
            </Tooltip>
          ))}
        </ToggleGroup>
      </div>
      {cannotChangeState && (
        <span role="status" className="muted basis-full pb-2 text-xs">
          Shorten this search to change its state filter.
        </span>
      )}
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
