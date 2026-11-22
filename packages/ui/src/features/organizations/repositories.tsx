import { RepoIcon } from "@primer/octicons-react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Badge } from "@/components/ui/badge";
import { GitImport } from "@/features/imports";
import { CreateRepository } from "@/features/repository/create";
import { navigate, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";

export function OrganizationRepositories({
  id,
  name,
  actor,
  canCreate,
  requestedImport,
  csrf,
}: {
  id: string;
  name: string;
  actor: string;
  canCreate: boolean;
  requestedImport?: string;
  csrf: string;
}) {
  const repositories = useRequest<{ repositories: Repository[] }>(
    `/api/repos?${new URLSearchParams({ organization: id })}`,
  );
  return (
    <section className="grid gap-4" aria-labelledby="organization-repositories">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <h2 id="organization-repositories" className="text-lg font-semibold">
          Organization repositories
        </h2>
        {canCreate && (
          <div className="flex flex-wrap items-start gap-2">
            <CreateRepository
              key={`${id}:${actor}`}
              organization={{ id, name, actor }}
              csrf={csrf}
              onCreated={(repo) => navigate(repoHref(repo))}
            />
            <GitImport
              key={`${id}:${actor}:${requestedImport ?? ""}`}
              account={actor}
              organization={{ id, name, requestedJob: requestedImport }}
              csrf={csrf}
              onImported={(repo) => navigate(repoHref(repo))}
            />
          </div>
        )}
      </header>
      <Result state={repositories} timingLabel="Organization repositories">
        {({ repositories: items }) =>
          items.length ? (
            <ul className="divide-y rounded-lg border">
              {items.map((repo) => (
                <li key={repo.id} className="flex items-start gap-3 p-4">
                  <RepoIcon
                    aria-hidden="true"
                    className="mt-1 shrink-0 text-muted-foreground"
                  />
                  <div className="min-w-0">
                    <div className="flex flex-wrap items-center gap-2">
                      <Link
                        className="break-words font-semibold underline-offset-4 hover:underline"
                        href={repoHref(repo)}
                      >
                        {repo.name}
                      </Link>
                      {repo.archived && (
                        <Badge variant="outline">Archived</Badge>
                      )}
                    </div>
                    {repo.description && (
                      <p className="mt-1 break-words text-sm text-muted-foreground">
                        {repo.description}
                      </p>
                    )}
                  </div>
                </li>
              ))}
            </ul>
          ) : (
            <p className="rounded-lg border p-6 text-center text-sm text-muted-foreground">
              No repositories are visible to you in this organization.
            </p>
          )
        }
      </Result>
    </section>
  );
}
