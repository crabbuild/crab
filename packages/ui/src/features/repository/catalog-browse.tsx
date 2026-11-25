import { RepoIcon } from "@primer/octicons-react";
import { SettingsIcon } from "lucide-react";
import { useRef } from "react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { navigate, useRequest, type Repository } from "@/lib/api";
import type { RepositoryAccess } from "@/lib/contracts";
import { repositoryRoleLabels } from "@/lib/repository-roles";
import { repoHref } from "@/lib/repository-route";
import { RepositoryPropertyFilters } from "./catalog-property-filters";
import { RepositoryStateFilters } from "./catalog-state-filters";
import { TopicLinks } from "./topic-links";

type Entry = Pick<
  Repository,
  "id" | "owner" | "name" | "description" | "archived" | "topics" | "visibility"
>;
type Page = {
  repositories: (Entry & {
    fork: boolean;
    can_admin?: boolean;
    team_access?: RepositoryAccess;
    can_manage_team_access?: boolean;
  })[];
  next: string | null;
};

type Scope =
  | { kind: "all"; subject?: string }
  | { kind: "public" }
  | { kind: "personal" }
  | { kind: "profile"; actor: string }
  | { kind: "organization"; organization: string; owner: boolean }
  | { kind: "team"; organization: string; team: string };

function catalogPath(scope: Scope) {
  switch (scope.kind) {
    case "personal":
      return "/api/repos/personal";
    case "profile":
      return `/api/actors/${encodeURIComponent(scope.actor)}/repositories`;
    case "team":
      return `/api/organizations/${encodeURIComponent(scope.organization)}/teams/${encodeURIComponent(scope.team)}/repositories`;
    default:
      return "/api/repos";
  }
}

const emptyText = {
  public: "There are no public repositories yet.",
  personal: "You don’t have any personal repositories yet.",
  profile: "No repositories owned by this user are visible to you.",
  organization: "No repositories are visible to you in this organization.",
  team: "No repositories with team access are visible to you.",
};

export function BrowseRepositories({ url, scope }: { url: URL; scope: Scope }) {
  const query = url.searchParams.get("q") ?? "";
  const after = url.searchParams.get("after");
  const params = new URLSearchParams({ limit: "32" });
  if (query) params.set("q", query);
  if (after) params.set("after", after);
  if (scope.kind === "public") params.set("public", "true");
  const organization =
    scope.kind === "organization" || scope.kind === "team"
      ? scope.organization
      : undefined;
  if (scope.kind === "organization")
    params.set("organization", scope.organization);
  const page = useRequest<Page>(`${catalogPath(scope)}?${params}`);
  const scoped = scope.kind !== "all";
  const RepositoryHeading =
    scope.kind === "team" ? "h4" : scope.kind === "organization" ? "h3" : "h2";
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
          className="min-w-36 max-w-md flex-1"
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
      <Result state={page} timingLabel="Repositories">
        {({ repositories, next }) => (
          <>
            {repositories.length ? (
              <ul
                className={scoped ? "divide-y rounded-lg border" : "repo-cards"}
                aria-label="Repository results"
              >
                {repositories.map((repo) => (
                  <li
                    key={repo.id}
                    className={scoped ? "min-w-0 p-4" : "panel repo-card"}
                  >
                    <div
                      className={
                        scoped
                          ? "flex flex-wrap items-center gap-2"
                          : "repo-card-heading"
                      }
                    >
                      <RepoIcon
                        size={18}
                        className="shrink-0 text-muted-foreground"
                        aria-hidden="true"
                      />
                      <RepositoryHeading
                        className={
                          scoped
                            ? "min-w-0 wrap-anywhere text-base font-semibold"
                            : undefined
                        }
                      >
                        <Link href={repoHref(repo)}>
                          {!scoped && <>{repo.owner} / </>}
                          <strong>{repo.name}</strong>
                        </Link>
                      </RepositoryHeading>
                      {repo.archived && (
                        <Badge variant="outline">Archived</Badge>
                      )}
                      {repo.fork && <Badge variant="outline">Fork</Badge>}
                      {repo.visibility === "public" && (
                        <Badge variant="outline">Public</Badge>
                      )}
                      {repo.team_access && (
                        <Badge variant="secondary">
                          {repositoryRoleLabels[repo.team_access]}
                        </Badge>
                      )}
                      {scope.kind === "organization" && repo.can_admin && (
                        <Button
                          asChild
                          variant="ghost"
                          size="sm"
                          className="sm:ms-auto"
                        >
                          <Link
                            href={repoHref(repo, { view: "settings" })}
                            aria-label={`Settings for ${repo.name}`}
                          >
                            <SettingsIcon data-icon="inline-start" />
                            Settings
                          </Link>
                        </Button>
                      )}
                      {(repo.can_manage_team_access ||
                        (scope.kind === "organization" &&
                          scope.owner &&
                          !repo.fork)) && (
                        <Button
                          asChild
                          variant="ghost"
                          size="sm"
                          className={
                            scope.kind === "organization" && repo.can_admin
                              ? undefined
                              : "sm:ms-auto"
                          }
                        >
                          <Link
                            href={repoHref(repo, {
                              view: "settings",
                              section: "team-access",
                            })}
                            aria-label={`Manage team access for ${repo.name}`}
                          >
                            Manage team access
                          </Link>
                        </Button>
                      )}
                    </div>
                    {repo.description && (
                      <p dir="auto" className="muted mt-1 wrap-anywhere">
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
            ) : scope.kind !== "all" ? (
              <p
                role="status"
                className="rounded-lg border p-6 text-center text-sm text-muted-foreground"
              >
                {emptyText[scope.kind]}
              </p>
            ) : (
              <div className="notice">
                <h2>No repositories available</h2>
                <p>
                  Create a repository, import one from Git, or ask an
                  administrator for access.
                </p>
                {scope.subject && (
                  <p className="muted wrap-anywhere">
                    Your user ID: <code>{scope.subject}</code>
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
