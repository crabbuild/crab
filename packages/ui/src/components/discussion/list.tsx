import { useLayoutEffect, useRef, useState, type ComponentProps } from "react";
import {
  AlignJustifyIcon,
  ArrowDownIcon,
  ArrowUpIcon,
  CalendarPlusIcon,
  ClockIcon,
  EllipsisIcon,
  ListFilterIcon,
  SmileIcon,
  Rows2Icon,
  ChevronDownIcon,
  MessageSquareIcon,
  ArrowDownWideNarrowIcon,
  CheckIcon,
} from "lucide-react";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import { ButtonGroup } from "@/components/ui/button-group";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuLabel,
  DropdownMenuPortal,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { navigate, type Repository, type RepositoryLabel } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { DiscussionFilter, type ConversationOptions } from "./filter";

export type ListLabel = Pick<RepositoryLabel, "id" | "name" | "color">;
export interface DiscussionPage<T> {
  repository_id: string;
  can_bulk_edit: boolean;
  items: T[];
  next: string | null;
  counts: { open: number; closed: number; merged: number };
  matching: number;
  authored_by_me: boolean;
  assigned_to_me: boolean;
  review_requested_from_me: boolean;
  lock_filter?: ConversationOptions;
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
  ["created", "Created", CalendarPlusIcon],
  ["updated", "Updated", ClockIcon],
  ["comments", "Comments", MessageSquareIcon],
  ["relevance", "Relevance", ListFilterIcon],
] as const;
const reactionSorts = [
  ["reactions", "Total reactions"],
  ["thumbs_up", "👍 reactions"],
  ["thumbs_down", "👎 reactions"],
  ["laugh", "😄 reactions"],
  ["hooray", "🎉 reactions"],
  ["confused", "😕 reactions"],
  ["heart", "❤️ reactions"],
  ["rocket", "🚀 reactions"],
  ["eyes", "👀 reactions"],
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
  selecting,
}: {
  repo: Repository;
  url: URL;
  view: "issues" | "pulls";
  data?: Pick<
    DiscussionPage<unknown>,
    "counts" | "navigation" | "matching" | "lock_filter"
  >;
  loading: boolean;
  density: "comfortable" | "compact";
  onDensityChange: (density: "comfortable" | "compact") => void;
  selecting: boolean;
}) {
  const filters = discussionFilters(url);
  const [sortOpen, setSortOpen] = useState(false);
  const toolbar = useRef<HTMLDivElement>(null);
  const [layout, setLayout] = useState<"full" | "overflow" | "compact">("full");
  useLayoutEffect(() => {
    const element = toolbar.current;
    if (!element) return;
    // The sidebar and diagnostic panels can narrow the list independently of the window.
    const update = () => {
      const width = element.clientWidth;
      if (!width) return;
      setLayout(
        width < 320
          ? "compact"
          : width < (view === "pulls" ? 900 : 840)
            ? "overflow"
            : "full",
      );
    };
    const observer = new ResizeObserver(update);
    observer.observe(element);
    update();
    return () => observer.disconnect();
  }, [view]);
  const navigation = data?.navigation;
  const lock = data?.lock_filter;
  const [supportsConversationFilter, setSupportsConversationFilter] =
    useState(false);
  // Keep the trigger mounted when its query refreshes so focus and adjacent anchors stay put.
  if (data && supportsConversationFilter !== !!lock) {
    setSupportsConversationFilter(!!lock);
  }
  const cannotChangeState =
    navigation?.kind === "state" &&
    (navigation.open === null || navigation.closed === null);
  const sortLabel = [...sorts, ...reactionSorts].find(
    ([key]) => key === filters.sort,
  )?.[1];
  const reactionSelected = reactionSorts.some(([key]) => key === filters.sort);
  const query = discussionQuery(url, view);
  const change = (values: Partial<typeof filters>) =>
    navigate(repoHref(repo, { view, ...filters, ...values }));
  const sortText =
    filters.sort === "created"
      ? filters.direction === "asc"
        ? "Oldest"
        : "Newest"
      : (sortLabel ?? "Sort");
  function filterButton(
    kind: ComponentProps<typeof DiscussionFilter>["kind"],
    inOverflow = false,
  ) {
    return (
      <DiscussionFilter
        key={kind}
        kind={kind}
        repo={repo}
        query={query}
        inOverflow={inOverflow}
        conversation={kind === "conversations" ? lock : undefined}
        onSelect={(q) => change({ q })}
      />
    );
  }
  const secondaryFilters = (
    <>
      {filterButton("milestones", layout !== "full")}
      {filterButton("assignees", layout !== "full")}
      {view === "issues" &&
        repo.organization_id &&
        filterButton("issue-types", layout !== "full")}
      {(lock || (loading && supportsConversationFilter)) &&
        filterButton("conversations", layout !== "full")}
      {view === "pulls" &&
        !repo.public_read_only &&
        filterButton("reviews", layout !== "full")}
    </>
  );
  return (
    <div ref={toolbar} className="issues-filters flex-wrap" inert={selecting}>
      <div className="discussion-counts gap-3">
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
            <ButtonGroup className="discussion-segments">
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
                  <Button asChild variant="ghost" size="sm" key={state}>
                    <Link
                      aria-current={
                        navigation.selected === state ? "page" : undefined
                      }
                      href={repoHref(repo, { view, ...filters, q: query })}
                    >
                      {label}
                    </Link>
                  </Button>
                );
              })}
            </ButtonGroup>
          </nav>
        ) : (
          <h3 className="m-0 text-sm font-medium">
            {new Intl.NumberFormat().format(data.matching)}{" "}
            {data.matching === 1 ? "result" : "results"}
          </h3>
        )}
      </div>
      <div className="discussion-toolbar-actions ml-auto flex min-w-0 max-w-full flex-wrap items-center gap-2">
        <div
          role="group"
          aria-label="Discussion filters"
          className="flex min-w-0 max-w-full flex-wrap items-center gap-2 p-1 -m-1"
        >
          {layout !== "compact" && filterButton("authors")}
          {layout !== "compact" && filterButton("labels")}
          {layout !== "full" ? (
            <Popover>
              <PopoverTrigger asChild>
                <Button variant="outline" size="sm" aria-label="More filters">
                  <EllipsisIcon data-icon="inline-start" aria-hidden="true" />
                  <span data-filter-label>More</span>
                </Button>
              </PopoverTrigger>
              <PopoverContent
                align="end"
                collisionPadding={12}
                aria-label="More filters"
                className="discussion-filter-overflow w-48 gap-1"
              >
                {layout === "compact" && filterButton("authors", true)}
                {layout === "compact" && filterButton("labels", true)}
                {secondaryFilters}
              </PopoverContent>
            </Popover>
          ) : (
            secondaryFilters
          )}
          <DropdownMenu
            modal={false}
            open={sortOpen}
            onOpenChange={setSortOpen}
          >
            <DropdownMenuTrigger asChild>
              <Button
                variant="outline"
                size="sm"
                aria-label="Sort discussions"
                title={`Sort discussions · ${sortText}`}
              >
                <ArrowDownWideNarrowIcon
                  data-icon="inline-start"
                  aria-hidden="true"
                />
                <span data-filter-label>{sortText}</span>{" "}
                <ChevronDownIcon data-icon="inline-end" />
              </Button>
            </DropdownMenuTrigger>
            {/* Closing scopes must unmount before a rapid reopen during refresh. */}
            {sortOpen && (
              <DropdownMenuContent align="end" className="w-48" tabIndex={0}>
                <DropdownMenuGroup>
                  <DropdownMenuLabel>Sort by</DropdownMenuLabel>
                  <DropdownMenuRadioGroup
                    value={filters.sort}
                    onValueChange={(sort) => change({ sort })}
                  >
                    {sorts.map(([value, label, Icon]) => (
                      <DropdownMenuRadioItem key={value} value={value}>
                        <Icon
                          aria-hidden="true"
                          className="text-muted-foreground"
                        />
                        {label}
                      </DropdownMenuRadioItem>
                    ))}
                  </DropdownMenuRadioGroup>
                  <DropdownMenuSub>
                    <DropdownMenuSubTrigger>
                      <SmileIcon
                        aria-hidden="true"
                        className="text-muted-foreground"
                      />
                      Reactions
                      {reactionSelected && <CheckIcon aria-hidden="true" />}
                    </DropdownMenuSubTrigger>
                    <DropdownMenuPortal>
                      <DropdownMenuSubContent className="w-48">
                        <DropdownMenuRadioGroup
                          value={filters.sort}
                          onValueChange={(sort) => change({ sort })}
                        >
                          {reactionSorts.map(([value, label]) => (
                            <DropdownMenuRadioItem key={value} value={value}>
                              {label}
                            </DropdownMenuRadioItem>
                          ))}
                        </DropdownMenuRadioGroup>
                      </DropdownMenuSubContent>
                    </DropdownMenuPortal>
                  </DropdownMenuSub>
                </DropdownMenuGroup>
                <DropdownMenuSeparator />
                <DropdownMenuLabel>Order</DropdownMenuLabel>
                <DropdownMenuRadioGroup
                  value={filters.direction}
                  onValueChange={(direction) => change({ direction })}
                >
                  <DropdownMenuRadioItem value="desc">
                    <ArrowDownIcon
                      aria-hidden="true"
                      className="text-muted-foreground"
                    />
                    {filters.sort === "created" || filters.sort === "updated"
                      ? "Newest first"
                      : "Highest first"}
                  </DropdownMenuRadioItem>
                  <DropdownMenuRadioItem value="asc">
                    <ArrowUpIcon
                      aria-hidden="true"
                      className="text-muted-foreground"
                    />
                    {filters.sort === "created" || filters.sort === "updated"
                      ? "Oldest first"
                      : "Lowest first"}
                  </DropdownMenuRadioItem>
                </DropdownMenuRadioGroup>
              </DropdownMenuContent>
            )}
          </DropdownMenu>
        </div>
        <ToggleGroup
          className="discussion-segments"
          type="single"
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
