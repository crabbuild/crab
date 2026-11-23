import { RepoIcon } from "@primer/octicons-react";
import { useRef } from "react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { navigate, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { RepositoryPropertyFilters } from "./catalog-property-filters";
import { RepositoryStateFilters } from "./catalog-state-filters";
import { TopicLinks } from "./topic-links";

type Entry = Pick<
  Repository,
  "id" | "owner" | "name" | "description" | "archived" | "topics"
>;
type Page = {
  repositories: (Entry & { fork: boolean })[];
  next: string | null;
};

export function BrowseRepositories({
  url,
  organization,
  subject,
}: {
  url: URL;
  organization?: string;
  subject?: string;
}) {
  const query = url.searchParams.get("q") ?? "";
  const after = url.searchParams.get("after");
  const params = new URLSearchParams({ limit: "32" });
  if (query) params.set("q", query);
  if (after) params.set("after", after);
  if (organization) params.set("organization", organization);
  const page = useRequest<Page>(`/api/repos?${params}`);
  const searchInput = useRef<HTMLInputElement>(null);

  function href(search: string, next?: string) {
    const destination = new URL(url);
    destination.searchParams.delete("q");
    destination.searchParams.delete("after");
    if (search) destination.searchParams.set("q", search);
    if (next) destination.searchParams.set("after", next);
    return destination.pathname + destination.search;
  }

  const draft = () => searchInput.current?.value.trim() ?? query;
  function apply(search: string) {
    if (search.length > 200) return false;
    navigate(href(search));
    return true;
  }

  return (
    <div className="grid min-w-0 gap-4">
      <form
        className="flex flex-wrap items-center gap-2"
        aria-label="Repository search"
        role="search"
        onSubmit={(event) => {
          event.preventDefault();
          const value = new FormData(event.currentTarget).get("q");
          const destination = href(
            typeof value === "string" ? value.trim() : "",
          );
          if (destination === url.pathname + url.search) page.retry();
          else navigate(destination);
        }}
      >
        <Input
          key={query}
          ref={searchInput}
          type="search"
          className="max-w-md flex-1"
          name="q"
          aria-label="Search repositories"
          placeholder={
            organization
              ? "Search names or use topic: and props. filters"
              : "Search names, descriptions, or topic:name"
          }
          maxLength={200}
          defaultValue={query}
        />
        <Button type="submit" size="sm">
          Search
        </Button>
        {query && (
          <Button asChild variant="ghost" size="sm">
            <Link href={href("")}>Clear search</Link>
          </Button>
        )}
      </form>
      <RepositoryStateFilters
        key={query}
        query={query}
        draft={draft}
        apply={apply}
      />
      {organization && (
        <RepositoryPropertyFilters
          organization={organization}
          apply={(filter) => {
            const text = draft();
            return apply(text ? `${text} ${filter}` : filter);
          }}
        />
      )}
      <Result state={page} timingLabel="Repository search">
        {({ repositories, next }) => (
          <>
            {repositories.length ? (
              <ul
                className={
                  organization ? "divide-y rounded-lg border" : "repo-cards"
                }
                aria-label="Repository results"
              >
                {repositories.map((repo) => (
                  <li
                    key={repo.id}
                    className={organization ? "p-4" : "panel repo-card"}
                  >
                    <div className="repo-card-heading">
                      <RepoIcon size={18} aria-hidden="true" />
                      <h2>
                        <Link href={repoHref(repo)}>
                          {!organization && <>{repo.owner} / </>}
                          <strong>{repo.name}</strong>
                        </Link>
                      </h2>
                      {repo.archived && (
                        <Badge variant="outline">Archived</Badge>
                      )}
                      {repo.fork && <Badge variant="outline">Fork</Badge>}
                    </div>
                    {repo.description && (
                      <p className="muted mt-1 break-words">
                        {repo.description}
                      </p>
                    )}
                    <TopicLinks
                      names={repo.topics}
                      href={(topic) => href(`topic:${topic}`)}
                    />
                  </li>
                ))}
              </ul>
            ) : next ? (
              <p role="status" className="notice">
                No matches on this page. More repositories are available.
              </p>
            ) : query || after ? (
              <p role="status" className="notice">
                No repositories match this search.
              </p>
            ) : organization ? (
              <p className="rounded-lg border p-6 text-center text-sm text-muted-foreground">
                No repositories are visible to you in this organization.
              </p>
            ) : (
              <div className="notice">
                <h2>No repositories available</h2>
                <p>
                  Create a repository, import one from Git, or ask an
                  administrator for access.
                </p>
                {subject && (
                  <p className="muted">
                    Your user ID: <code>{subject}</code>
                  </p>
                )}
              </div>
            )}
            {next && (
              <Button asChild variant="outline" className="justify-self-end">
                <Link href={href(query, next)}>Next page</Link>
              </Button>
            )}
          </>
        )}
      </Result>
      {after && (
        <Button asChild variant="ghost" className="justify-self-start">
          <Link href={href(query)}>Back to first page</Link>
        </Button>
      )}
    </div>
  );
}
