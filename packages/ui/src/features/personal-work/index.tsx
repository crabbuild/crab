import {
  GitMergeIcon,
  GitPullRequestClosedIcon,
  GitPullRequestDraftIcon,
  GitPullRequestIcon,
  IssueClosedIcon,
  IssueOpenedIcon,
} from "@primer/octicons-react";
import { RelativeTime } from "@/components/discussion/time";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";

type Kind = "issue" | "pull";
type State = "open" | "closed" | "all";
type Relation = "authored" | "assigned" | "review_requested";
type Filter = { kind: Kind; relation: Relation; state: State };

type Item = {
  repository: Pick<Repository, "id" | "owner" | "name">;
  number: number;
  title: string;
  state: "open" | "closed" | "merged";
  draft: boolean;
  created_at: number;
};

function query(filter: Filter, after?: string) {
  return new URLSearchParams(after ? { ...filter, after } : filter).toString();
}

function href(filter: Filter, after?: string) {
  return `/-/work?${query(filter, after)}`;
}

const relations: [Relation, string][] = [
  ["authored", "Created by me"],
  ["assigned", "Assigned to me"],
  ["review_requested", "Review requests"],
];

const states: [State, string][] = [
  ["open", "Open"],
  ["closed", "Closed"],
  ["all", "All states"],
];

function WorkRow({ kind, item }: { kind: Kind; item: Item }) {
  const label =
    item.draft && item.state === "open"
      ? "Draft"
      : (
          {
            open: "Open",
            closed: "Closed",
            merged: "Merged",
          } as const
        )[item.state];
  const Icon =
    kind === "issue"
      ? item.state === "open"
        ? IssueOpenedIcon
        : IssueClosedIcon
      : {
          Open: GitPullRequestIcon,
          Draft: GitPullRequestDraftIcon,
          Closed: GitPullRequestClosedIcon,
          Merged: GitMergeIcon,
        }[label];
  const location =
    kind === "issue"
      ? { view: "issues", issue: String(item.number) }
      : { view: "pulls", pull: String(item.number) };
  const { repository } = item;
  return (
    <li className="grid min-w-0 grid-cols-[auto_minmax(0,1fr)] gap-x-3 px-4 py-3 sm:grid-cols-[auto_minmax(0,1fr)_auto]">
      <Icon aria-hidden="true" className="mt-1 text-muted-foreground" />
      <div className="min-w-0">
        <Link
          className="break-words font-medium hover:underline"
          href={repoHref(repository, {
            ...location,
            repository_id: repository.id,
          })}
        >
          {item.title}
        </Link>
        <p className="m-0 mt-1 break-words text-sm text-muted-foreground">
          <Link
            href={repoHref(repository, { repository_id: repository.id })}
            className="hover:underline"
          >
            {repository.owner}/{repository.name}
          </Link>
          {" #"}
          {item.number}
        </p>
      </div>
      <p className="col-start-2 m-0 mt-1 text-sm text-muted-foreground sm:col-start-3">
        {label} · Opened <RelativeTime value={item.created_at} />
      </p>
    </li>
  );
}

export function PersonalWork({ url }: { url: URL }) {
  const kind = url.searchParams.get("kind") === "issue" ? "issue" : "pull";
  const relationValue = url.searchParams.get("relation");
  const relation =
    relationValue === "assigned" ||
    (kind === "pull" && relationValue === "review_requested")
      ? relationValue
      : "authored";
  const stateValue = url.searchParams.get("state");
  const state =
    relation !== "review_requested" &&
    (stateValue === "closed" || stateValue === "all")
      ? stateValue
      : "open";
  const filter = { kind, relation, state } satisfies Filter;
  const after = url.searchParams.get("after") ?? undefined;
  const result = useRequest<{ items: Item[]; next: string | null }>(
    `/api/personal-work?${query(filter, after)}`,
  );
  return (
    <section
      className="mx-auto grid w-full min-w-0 max-w-5xl gap-5 p-4 md:p-6"
      aria-labelledby="work-heading"
    >
      <h1 id="work-heading" className="m-0 text-2xl font-semibold">
        My work
      </h1>
      <nav className="flex flex-wrap gap-2" aria-label="Work type">
        {(["pull", "issue"] as const).map((value) => (
          <Button
            asChild
            key={value}
            variant={value === kind ? "secondary" : "ghost"}
          >
            <Link
              aria-current={value === kind ? "page" : undefined}
              href={href({
                ...filter,
                kind: value,
                relation:
                  value === "issue" && relation === "review_requested"
                    ? "authored"
                    : relation,
              })}
            >
              {value === "pull" ? "Pull requests" : "Issues"}
            </Link>
          </Button>
        ))}
      </nav>
      <div className="flex flex-wrap items-center justify-between gap-3 border-b pb-3">
        <nav className="flex flex-wrap gap-1" aria-label="Your involvement">
          {relations
            .filter(
              ([value]) => kind === "pull" || value !== "review_requested",
            )
            .map(([value, name]) => (
              <Button
                asChild
                key={value}
                size="sm"
                variant={value === relation ? "secondary" : "ghost"}
              >
                <Link
                  aria-current={value === relation ? "page" : undefined}
                  href={href({
                    ...filter,
                    relation: value,
                    state: value === "review_requested" ? "open" : state,
                  })}
                >
                  {name}
                </Link>
              </Button>
            ))}
        </nav>
        {relation !== "review_requested" && (
          <nav className="flex flex-wrap gap-1" aria-label="Work state">
            {states.map(([value, name]) => (
              <Button
                asChild
                key={value}
                size="sm"
                variant={value === state ? "secondary" : "ghost"}
              >
                <Link
                  aria-current={value === state ? "page" : undefined}
                  href={href({ ...filter, state: value })}
                >
                  {name}
                </Link>
              </Button>
            ))}
          </nav>
        )}
      </div>
      <Result state={result} timingLabel="personal work">
        {({ items, next }) => (
          <>
            {items.length ? (
              <ul
                className="m-0 list-none divide-y rounded-lg border p-0"
                aria-label="Matching work, newest first"
              >
                {items.map((item) => (
                  <WorkRow
                    key={`${item.repository.id}:${item.number}`}
                    kind={kind}
                    item={item}
                  />
                ))}
              </ul>
            ) : (
              <p className="notice">
                No matching {kind === "pull" ? "pull requests" : "issues"}.
              </p>
            )}
            {(after || next) && (
              <nav
                className="flex flex-wrap justify-between gap-2"
                aria-label="Work pages"
              >
                {after && (
                  <Button asChild variant="outline">
                    <Link href={href(filter)}>First page</Link>
                  </Button>
                )}
                {next && (
                  <Button asChild variant="outline" className="ml-auto">
                    <Link href={href(filter, next)}>Next page</Link>
                  </Button>
                )}
              </nav>
            )}
          </>
        )}
      </Result>
      {after && result.error && (
        <Button asChild variant="outline" className="justify-self-start">
          <Link href={href(filter)}>First page</Link>
        </Button>
      )}
    </section>
  );
}
