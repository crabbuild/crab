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
import type { Repository, useRequest } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";

export type Kind = "issue" | "pull";
type Item = {
  repository: Pick<Repository, "id" | "owner" | "name">;
  kind: Kind;
  number: number;
  title: string;
  state: "open" | "closed" | "merged";
  draft: boolean;
  created_at: number;
  updated_at: number;
};
export type WorkPage = { items: Item[]; next: string | null };

function WorkRow({ item, showUpdated }: { item: Item; showUpdated: boolean }) {
  const { kind } = item;
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
          <span className="sr-only">
            {kind === "issue" ? " issue" : " pull request"}
          </span>
          {" #"}
          {item.number}
        </p>
      </div>
      <p className="col-start-2 m-0 mt-1 text-sm text-muted-foreground sm:col-start-3">
        {label} · {showUpdated ? "Updated" : "Opened"}{" "}
        <RelativeTime value={showUpdated ? item.updated_at : item.created_at} />
      </p>
    </li>
  );
}

export function WorkResults({
  result,
  after,
  href,
  emptyMessage,
  showUpdated = false,
}: {
  result: ReturnType<typeof useRequest<WorkPage>>;
  after?: string;
  href: (after?: string) => string;
  emptyMessage: string;
  showUpdated?: boolean;
}) {
  return (
    <>
      <Result state={result} timingLabel="work list">
        {({ items, next }) => (
          <>
            {items.length ? (
              <ul
                className="m-0 list-none divide-y rounded-lg border p-0"
                aria-label="Matching work"
              >
                {items.map((item) => (
                  <WorkRow
                    key={`${item.repository.id}:${item.kind}:${item.number}`}
                    showUpdated={showUpdated}
                    item={item}
                  />
                ))}
              </ul>
            ) : (
              <p className="notice">{emptyMessage}</p>
            )}
            {(after || next) && (
              <nav
                className="flex flex-wrap justify-between gap-2"
                aria-label="Work pages"
              >
                {after && (
                  <Button asChild variant="outline">
                    <Link href={href()}>First page</Link>
                  </Button>
                )}
                {next && (
                  <Button asChild variant="outline" className="ml-auto">
                    <Link href={href(next)}>Next page</Link>
                  </Button>
                )}
              </nav>
            )}
          </>
        )}
      </Result>
      {after && result.error && (
        <Button asChild variant="outline" className="justify-self-start">
          <Link href={href()}>First page</Link>
        </Button>
      )}
    </>
  );
}
