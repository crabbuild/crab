import { GitPullRequestIcon } from "@primer/octicons-react";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";

export function RecentPushes({ repo }: { repo: Repository }) {
  const { data } = useRequest<{
    base_ref: string;
    items: { name: string; oid: string }[];
  }>(endpoint(repo, "pulls/recent-pushes"), repo.id);

  if (!data?.items.length) return null;
  return (
    <section
      className="mx-4 my-3 divide-y rounded-lg border bg-card"
      aria-label="Recent branch pushes"
    >
      {data.items.map(({ name }) => {
        const branch = name.slice("refs/heads/".length);
        const href = repo.forked_from
          ? repoHref(repo, {
              view: "fork-compare",
              rev: name,
              repository_id: repo.id,
            })
          : repoHref(repo, {
              view: "pulls",
              pull: "new",
              base: data.base_ref,
              head: name,
              repository_id: repo.id,
            });
        return (
          <div
            key={name}
            className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 p-3"
          >
            <p className="flex min-w-0 items-start gap-2 text-sm">
              <GitPullRequestIcon
                aria-hidden="true"
                className="mt-0.5 shrink-0 text-muted-foreground"
              />
              <span className="min-w-0 break-words">
                You recently pushed <code>{branch}</code>.
              </span>
            </p>
            <Button asChild size="sm" variant="outline">
              <Link
                href={href}
                aria-label={`Compare & pull request for ${branch}`}
              >
                Compare &amp; pull request
              </Link>
            </Button>
          </div>
        );
      })}
    </section>
  );
}
