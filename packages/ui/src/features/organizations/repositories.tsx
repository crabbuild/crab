import { Button } from "@/components/ui/button";
import { Link } from "@/components/navigation/link";
import { GitImport } from "@/features/imports";
import { BrowseRepositories } from "@/features/repository/catalog-browse";
import { CreateRepository } from "@/features/repository/create";
import { navigate } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";

export function OrganizationRepositories({
  id,
  name,
  actor,
  canCreate,
  url,
  csrf,
}: {
  id: string;
  name: string;
  actor: string;
  canCreate: boolean;
  url: URL;
  csrf: string;
}) {
  const requestedImport = url.searchParams.get("import") ?? undefined;
  return (
    <section className="grid gap-4" aria-labelledby="organization-repositories">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <h2 id="organization-repositories" className="text-lg font-semibold">
          Organization repositories
        </h2>
        <div className="flex flex-wrap items-start gap-2">
          <Button asChild variant="outline" size="sm">
            <Link
              href={`/-/code-search?${new URLSearchParams({ organization: id })}`}
            >
              Search organization code
            </Link>
          </Button>
          {canCreate && (
            <>
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
            </>
          )}
        </div>
      </header>
      <BrowseRepositories url={url} organization={id} />
    </section>
  );
}
