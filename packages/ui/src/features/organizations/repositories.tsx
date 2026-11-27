import { RepoIcon } from "@primer/octicons-react";
import { Button } from "@/components/ui/button";
import { Link } from "@/components/navigation/link";
import { GitImport } from "@/features/imports";
import { BrowseRepositories } from "@/features/repository/catalog-browse";
import { navigate } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";

export function OrganizationRepositories({
  id,
  name,
  actor,
  canCreate,
  owner,
  url,
  csrf,
}: {
  id: string;
  name: string;
  actor: string;
  canCreate: boolean;
  owner: boolean;
  url: URL;
  csrf: string;
}) {
  const requestedImport = url.searchParams.get("import") ?? undefined;
  return (
    <section className="grid gap-4" aria-labelledby="organization-repositories">
      <h2 id="organization-repositories" className="sr-only">
        Organization repositories
      </h2>
      <BrowseRepositories
        url={url}
        scope={{ kind: "organization", organization: id, owner }}
        actions={
          <>
            <Button asChild variant="ghost">
              <Link
                href={`/-/code-search?${new URLSearchParams({ organization: id })}`}
              >
                Search organization code
              </Link>
            </Button>
            {canCreate && (
              <Button asChild>
                <Link
                  href={`/-/new?${new URLSearchParams({ organization: id })}`}
                >
                  <RepoIcon data-icon="inline-start" />
                  New repository
                </Link>
              </Button>
            )}
            {(canCreate || requestedImport) && (
              <GitImport
                key={`${id}:${actor}:${requestedImport ?? ""}`}
                account={actor}
                organization={{
                  id,
                  name,
                  requestedJob: requestedImport,
                  canStart: canCreate,
                }}
                csrf={csrf}
                onImported={(repo) => navigate(repoHref(repo))}
              />
            )}
          </>
        }
      />
    </section>
  );
}
