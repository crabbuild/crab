import {
  CommentIcon,
  GitMergeIcon,
  GitPullRequestClosedIcon,
  GitPullRequestDraftIcon,
  GitPullRequestIcon,
  IssueClosedIcon,
  IssueOpenedIcon,
} from "@primer/octicons-react";
import { CheckIcon, ListFilterIcon } from "lucide-react";
import { useState } from "react";
import { Author } from "@/components/discussion/author";
import { RelativeTime } from "@/components/discussion/time";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Spinner } from "@/components/ui/spinner";
import type {
  Kind,
  WorkItem,
  WorkPage,
} from "@/features/personal-work/results";
import { useRequest } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { cn } from "@/lib/utils";

type FilterMode =
  | "all_open"
  | "authored"
  | "assigned"
  | "review_requested"
  | "closed";

const pullFilters: { id: FilterMode; label: string }[] = [
  { id: "all_open", label: "All open" },
  { id: "authored", label: "Created by me" },
  { id: "assigned", label: "Assigned to me" },
  { id: "review_requested", label: "Review requests" },
  { id: "closed", label: "Closed" },
];

const issueFilters: { id: FilterMode; label: string }[] = [
  { id: "all_open", label: "All open" },
  { id: "authored", label: "Created by me" },
  { id: "assigned", label: "Assigned to me" },
  { id: "closed", label: "Closed" },
];

function buildEndpoint(
  kind: Kind,
  mode: FilterMode,
  scopeOwner?: string,
): string {
  if (mode === "all_open" || mode === "closed") {
    const params = new URLSearchParams({
      q: mode === "all_open" ? "is:open" : "is:closed",
      kind,
      sort: "updated",
      limit: "6",
    });
    if (scopeOwner) params.set("scope", scopeOwner);
    return `/api/discussion-search?${params}`;
  }
  if (scopeOwner) {
    const relation =
      mode === "authored"
        ? "author:@me"
        : mode === "assigned"
          ? "assignee:@me"
          : "review-requested:@me";
    const params = new URLSearchParams({
      q: `is:open ${relation}`,
      kind,
      scope: scopeOwner,
      sort: "updated",
      limit: "6",
    });
    return `/api/discussion-search?${params}`;
  }
  const params = new URLSearchParams({
    kind,
    relation: mode,
    state: "open",
    sort: "updated",
    limit: "6",
  });
  return `/api/personal-work?${params}`;
}

function viewAllHref(kind: Kind, mode: FilterMode, scopeOwner?: string) {
  if (mode === "all_open" || mode === "closed" || scopeOwner) {
    const params = new URLSearchParams({
      q:
        mode === "closed"
          ? "is:closed"
          : mode === "authored"
            ? "is:open author:@me"
            : mode === "assigned"
              ? "is:open assignee:@me"
              : mode === "review_requested"
                ? "is:open review-requested:@me"
                : "is:open",
      kind,
      sort: "updated",
    });
    if (scopeOwner) params.set("scope", scopeOwner);
    return `/-/discussion-search?${params}`;
  }
  const params = new URLSearchParams({
    kind,
    relation: mode,
    state: "open",
  });
  return `/-/work?${params}`;
}

function StatusIcon({ item }: { item: WorkItem }) {
  if (item.kind === "issue") {
    if (item.state === "open") {
      return (
        <IssueOpenedIcon
          size={16}
          className="mt-0.5 shrink-0 text-(--success)"
          aria-label="Open issue"
        />
      );
    }
    return (
      <IssueClosedIcon
        size={16}
        className="mt-0.5 shrink-0 text-(--done)"
        aria-label="Closed issue"
      />
    );
  }
  if (item.state === "merged") {
    return (
      <GitMergeIcon
        size={16}
        className="mt-0.5 shrink-0 text-(--done)"
        aria-label="Merged pull request"
      />
    );
  }
  if (item.state === "closed") {
    return (
      <GitPullRequestClosedIcon
        size={16}
        className="mt-0.5 shrink-0 text-destructive"
        aria-label="Closed pull request"
      />
    );
  }
  if (item.draft) {
    return (
      <GitPullRequestDraftIcon
        size={16}
        className="mt-0.5 shrink-0 text-muted-foreground"
        aria-label="Draft pull request"
      />
    );
  }
  return (
    <GitPullRequestIcon
      size={16}
      className="mt-0.5 shrink-0 text-(--success)"
      aria-label="Open pull request"
    />
  );
}

export function HomeWorkSection({
  kind,
  scopeOwner,
}: {
  kind: Kind;
  scopeOwner?: string;
}) {
  const [filter, setFilter] = useState<FilterMode>("all_open");
  const title = kind === "pull" ? "Pull requests" : "Issues";
  const filters = kind === "pull" ? pullFilters : issueFilters;
  const activeFilter =
    filters.find((entry) => entry.id === filter) ?? filters[0];
  const state = useRequest<WorkPage>(buildEndpoint(kind, filter, scopeOwner));
  const headingId = `home-${kind}-heading`;

  return (
    <section aria-labelledby={headingId} className="grid gap-2.5">
      <div className="flex items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          <h2 id={headingId} className="m-0 text-sm font-semibold">
            {title}
          </h2>
          {filter !== "all_open" && (
            <span className="rounded-full border border-border bg-muted px-2 py-0.5 text-[11px] font-medium text-muted-foreground">
              {activeFilter.label}
            </span>
          )}
        </div>
        <div className="flex items-center gap-1.5">
          <Link
            href={viewAllHref(kind, filter, scopeOwner)}
            className="text-xs font-medium text-(--info) hover:underline"
          >
            View all
          </Link>
          <DropdownMenu modal={false}>
            <DropdownMenuTrigger asChild>
              <Button
                variant="ghost"
                size="icon-xs"
                aria-label={`Filter ${title.toLowerCase()}`}
                title={`Filter ${title.toLowerCase()} (${activeFilter.label})`}
                className="text-muted-foreground hover:text-foreground"
              >
                <ListFilterIcon className="size-3.5" aria-hidden="true" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="w-48">
              <DropdownMenuLabel>
                Filter {title.toLowerCase()}
              </DropdownMenuLabel>
              <DropdownMenuSeparator />
              {filters.map((option) => (
                <DropdownMenuItem
                  key={option.id}
                  onSelect={() => setFilter(option.id)}
                  className="justify-between"
                >
                  <span>{option.label}</span>
                  {filter === option.id && (
                    <CheckIcon className="size-3.5" aria-hidden="true" />
                  )}
                </DropdownMenuItem>
              ))}
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </div>

      <div className="overflow-hidden rounded-lg border border-border bg-card">
        {state.loading && !state.data ? (
          <div
            role="status"
            className="flex items-center gap-2 px-4 py-6 text-sm text-muted-foreground"
          >
            <Spinner /> Loading {title.toLowerCase()}…
          </div>
        ) : state.error ? (
          <div
            role="alert"
            className="flex flex-wrap items-center justify-between gap-2 px-4 py-4 text-sm text-muted-foreground"
          >
            <span>{state.error}</span>
            <Button variant="outline" size="xs" onClick={state.retry}>
              Retry
            </Button>
          </div>
        ) : state.data && state.data.items.length > 0 ? (
          <ul
            className="m-0 list-none divide-y divide-border p-0"
            aria-label={title}
          >
            {state.data.items.map((item) => {
              const { repository } = item;
              const href = repoHref(repository, {
                ...(kind === "pull"
                  ? { view: "pulls", pull: String(item.number) }
                  : { view: "issues", issue: String(item.number) }),
                repository_id: repository.id,
              });
              const updatedDifferent = item.updated_at > item.created_at;
              return (
                <li
                  key={`${repository.id}:${item.kind}:${item.number}`}
                  className="flex items-start gap-3 px-4 py-3 transition-colors hover:bg-muted/50"
                >
                  <StatusIcon item={item} />
                  <div className="min-w-0 flex-1">
                    <Link
                      href={href}
                      className="inline-flex min-h-6 max-w-full items-center truncate text-sm font-semibold text-foreground hover:text-(--info) hover:underline"
                    >
                      <span className="truncate">{item.title}</span>
                    </Link>
                    <p className="m-0 flex min-h-6 flex-wrap items-center gap-x-1 text-xs text-muted-foreground">
                      <span>
                        {repository.owner}/{repository.name}#{item.number}
                      </span>
                      {item.author && (
                        <>
                          <span aria-hidden="true">·</span>
                          <span>
                            Opened by{" "}
                            <span className="[&_a]:inline-flex [&_a]:min-h-6 [&_a]:items-center [&_a]:text-muted-foreground [&_a]:hover:text-foreground">
                              <Author author={item.author} />
                            </span>
                          </span>
                        </>
                      )}
                      <span aria-hidden="true">·</span>
                      <span>
                        {updatedDifferent ? "Updated" : "Created"}{" "}
                        <RelativeTime
                          value={
                            updatedDifferent ? item.updated_at : item.created_at
                          }
                        />
                      </span>
                    </p>
                  </div>
                  {item.comments !== undefined && item.comments > 0 && (
                    <Link
                      href={href}
                      className={cn(
                        "inline-flex min-h-6 min-w-6 shrink-0 items-center justify-center gap-1 text-xs text-muted-foreground hover:text-foreground",
                      )}
                      aria-label={`${item.comments} ${item.comments === 1 ? "comment" : "comments"}`}
                    >
                      <CommentIcon size={14} aria-hidden="true" />
                      <span>{item.comments}</span>
                    </Link>
                  )}
                </li>
              );
            })}
          </ul>
        ) : (
          <div className="flex flex-wrap items-center justify-between gap-2 px-4 py-5 text-sm text-muted-foreground">
            <span>
              No {activeFilter.label.toLowerCase()} {title.toLowerCase()} right
              now.
            </span>
            <Link
              href={
                kind === "pull"
                  ? "/-/work?kind=pull&state=all"
                  : "/-/work?kind=issue&state=all"
              }
              className="text-xs font-medium text-(--info) hover:underline"
            >
              Browse {title.toLowerCase()}
            </Link>
          </div>
        )}
      </div>
    </section>
  );
}
