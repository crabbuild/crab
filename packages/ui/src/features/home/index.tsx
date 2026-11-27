import {
  CodeIcon,
  OrganizationIcon,
  RepoCloneIcon,
  RepoForkedIcon,
  RepoIcon,
  SearchIcon,
} from "@primer/octicons-react";
import {
  ArrowRightIcon,
  ChevronDownIcon,
  ListTodoIcon,
  PlusIcon,
  SendHorizontalIcon,
} from "lucide-react";
import { useState, type FormEvent } from "react";
import { RelativeTime } from "@/components/discussion/time";
import { Link } from "@/components/navigation/link";
import { Badge } from "@/components/ui/badge";
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
import { reasons, type Reason } from "@/features/notifications/filters";
import type { OrganizationMembershipEntry } from "@/features/organizations/choices";
import { useOrganizationDirectory } from "@/features/organizations/directory";
import { navigate, useRequest, type Repository } from "@/lib/api";
import type { AvatarConfig, OwnProfile, Session } from "@/lib/contracts";
import { repoHref } from "@/lib/repository-route";
import { cn } from "@/lib/utils";
import crabMark from "../../../../web/crab.optimized.svg";
import { HomeRail } from "./rail";
import { HomeSidebar, type HomeContext, type HomeTab } from "./sidebar";
import { HomeWorkSection } from "./work-section";

const PINNED_TAB_KEY = "crab-home-pinned-tab";

type SearchMode = "code" | "discussions" | "repositories";

type FeedNotice = {
  id: string;
  repository: { id: string; owner: string; name: string };
  title: string;
  href: string;
  sender: string;
  reason: Reason;
  read: boolean;
  updated: { id: string; created_at_ms: number };
};

type CatalogPage = {
  repositories: (Repository & {
    fork?: boolean;
    created_at_ms?: number;
    metadata_updated_at_ms?: number;
  })[];
  next: string | null;
};

function greetingForNow() {
  const hour = new Date().getHours();
  if (hour < 12) return "Good morning";
  if (hour < 18) return "Good afternoon";
  return "Good evening";
}

function readPinnedTab(): HomeTab {
  try {
    return localStorage.getItem(PINNED_TAB_KEY) === "feed" ? "feed" : "home";
  } catch {
    return "home";
  }
}

export function HomeDashboard({
  session,
  avatar,
  url,
}: {
  session: Session;
  avatar?: AvatarConfig | null;
  url: URL;
}) {
  const [pinnedTab, setPinnedTab] = useState<HomeTab>(readPinnedTab);
  const tabParam = url.searchParams.get("tab");
  const activeTab: HomeTab =
    tabParam === "feed" ? "feed" : tabParam === "home" ? "home" : pinnedTab;

  const [context, setContext] = useState<HomeContext>({ kind: "personal" });
  const newRepository = () =>
    navigate(
      context.kind === "organization" && context.entry.can_create
        ? `/-/new?${new URLSearchParams({ organization: context.entry.organization.id })}`
        : "/-/new",
    );

  const ownProfile = useRequest<OwnProfile>(
    session.authenticated ? "/api/profile" : null,
  );
  const organizationDirectory = useOrganizationDirectory(
    undefined,
    session.authenticated,
  );
  const organizations: OrganizationMembershipEntry[] = (
    organizationDirectory.data?.items ?? []
  ).filter(
    (entry): entry is OrganizationMembershipEntry => entry.kind === "member",
  );

  const repoParams = new URLSearchParams({ limit: "32" });
  if (context.kind === "organization") {
    repoParams.set("organization", context.entry.organization.id);
  }
  const repositoryCatalog = useRequest<CatalogPage>(`/api/repos?${repoParams}`);
  const repositories = repositoryCatalog.data?.repositories ?? [];

  const accountName =
    ownProfile.data?.profile?.name ?? session.user?.name ?? "Developer";
  const accountHandle = ownProfile.data?.profile?.handle;
  const greetingName = accountHandle || accountName;

  const selectTab = (tab: HomeTab) => {
    navigate(tab === "feed" ? "/?tab=feed" : "/");
  };

  const pinTab = (tab: HomeTab) => {
    setPinnedTab(tab);
    try {
      localStorage.setItem(PINNED_TAB_KEY, tab);
    } catch {
      // Ignore storage failures.
    }
    selectTab(tab);
  };

  const scopeOwner =
    context.kind === "organization"
      ? `org-${context.entry.organization.id.replaceAll("-", "")}`
      : undefined;

  return (
    <div className="home-shell">
      <HomeSidebar
        accountName={accountName}
        accountHandle={accountHandle}
        accountAvatar={
          avatar ??
          ownProfile.data?.avatar ??
          ownProfile.data?.profile?.avatar ??
          null
        }
        activeTab={activeTab}
        pinnedTab={pinnedTab}
        onSelectTab={selectTab}
        onPinTab={pinTab}
        context={context}
        onSelectContext={setContext}
        organizations={organizations}
        repositories={repositories}
        repositoriesLoading={repositoryCatalog.loading}
        repositoriesError={repositoryCatalog.error}
        hasMoreRepositories={Boolean(repositoryCatalog.data?.next)}
        onRetryRepositories={repositoryCatalog.retry}
        onNewRepository={newRepository}
      />

      <div className="home-content">
        <div className="home-main">
          {activeTab === "home" ? (
            <>
              <HomeCommandHero
                greetingName={greetingName}
                repositories={repositories}
                organization={
                  context.kind === "organization"
                    ? context.entry.organization
                    : undefined
                }
                onNewRepository={newRepository}
              />
              <HomeWorkSection kind="pull" scopeOwner={scopeOwner} />
              <HomeWorkSection kind="issue" scopeOwner={scopeOwner} />
            </>
          ) : (
            <HomeFeed repositories={repositories} />
          )}
        </div>

        <HomeRail organizations={organizations} />
      </div>
    </div>
  );
}

function HomeCommandHero({
  greetingName,
  repositories,
  organization,
  onNewRepository,
}: {
  greetingName: string;
  repositories: Repository[];
  organization?: { id: string; name: string };
  onNewRepository: () => void;
}) {
  const [query, setQuery] = useState("");
  const [mode, setMode] = useState<SearchMode>("code");
  const [selectedRepo, setSelectedRepo] = useState<Repository>();
  const [codeKind, setCodeKind] = useState<
    "any" | "content" | "path" | "symbol"
  >("any");

  const modeLabels: Record<SearchMode, string> = {
    code: "Code",
    discussions: "Issues & PRs",
    repositories: "Repositories",
  };

  const codeKindLabels = {
    any: "Contents & paths",
    content: "File contents",
    path: "File paths",
    symbol: "Symbols",
  } as const;

  const submit = (event?: FormEvent) => {
    event?.preventDefault();
    const trimmed = query.trim();
    if (mode === "code") {
      if (selectedRepo) {
        const params = new URLSearchParams();
        if (trimmed) params.set("q", trimmed);
        if (codeKind !== "any") params.set("kind", codeKind);
        navigate(
          `${repoHref(selectedRepo)}/code-search${params.size ? `?${params}` : ""}`,
        );
        return;
      }
      const params = new URLSearchParams();
      if (trimmed) params.set("q", trimmed);
      if (organization) params.set("organization", organization.id);
      if (codeKind !== "any") params.set("kind", codeKind);
      navigate(`/-/code-search${params.size ? `?${params}` : ""}`);
      return;
    }
    if (mode === "discussions") {
      const params = new URLSearchParams();
      if (trimmed) params.set("q", trimmed);
      if (selectedRepo) {
        params.set("scope", `${selectedRepo.owner}/${selectedRepo.name}`);
      } else if (organization) {
        params.set("scope", `org-${organization.id.replaceAll("-", "")}`);
      }
      navigate(`/-/discussion-search${params.size ? `?${params}` : ""}`);
      return;
    }
    const params = new URLSearchParams();
    if (trimmed) params.set("q", trimmed);
    navigate(`/-/repositories${params.size ? `?${params}` : ""}`);
  };

  const scopeLabel = selectedRepo
    ? `${selectedRepo.owner}/${selectedRepo.name}`
    : organization
      ? organization.name
      : "All repositories";

  return (
    <section aria-labelledby="home-greeting-heading" className="grid gap-3.5">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex min-w-0 items-center gap-2.5">
          <img
            src={crabMark}
            alt=""
            className="size-6 shrink-0 object-contain"
          />
          <h1
            id="home-greeting-heading"
            className="m-0 truncate text-xl font-semibold tracking-tight text-foreground sm:text-2xl"
          >
            {greetingForNow()}, {greetingName}!
          </h1>
        </div>
        <span className="inline-flex items-center rounded-full border border-(--success)/40 bg-(--success)/10 px-2.5 py-0.5 text-xs font-medium text-(--success)">
          Dashboard
        </span>
      </div>

      <form
        role="search"
        aria-label="Workspace quick search"
        onSubmit={submit}
        className="home-composer overflow-hidden rounded-xl border border-border bg-card shadow-xs transition-colors"
      >
        <div className="px-4 pt-3.5 pb-1.5">
          <textarea
            data-slot="home-composer-input"
            rows={2}
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey) {
                event.preventDefault();
                submit();
              }
            }}
            placeholder={
              mode === "code"
                ? "Search code across repositories, or press / to jump anywhere"
                : mode === "discussions"
                  ? "Search issues and pull requests (e.g. is:open label:bug)"
                  : "Find repositories by name, topic, or description"
            }
            aria-label="Workspace search query"
            className="block w-full resize-none border-0 bg-transparent p-0 text-sm leading-relaxed text-foreground shadow-none outline-none placeholder:text-muted-foreground"
          />
        </div>
        <div className="flex flex-wrap items-center justify-between gap-2 px-3 pt-1 pb-2.5">
          <div className="flex flex-wrap items-center gap-1.5">
            <DropdownMenu modal={false}>
              <DropdownMenuTrigger asChild>
                <Button
                  type="button"
                  variant="outline"
                  size="xs"
                  className="h-7 gap-1.5 bg-background/80 px-2.5 text-xs font-medium"
                >
                  <SearchIcon size={13} aria-hidden="true" />
                  <span>{modeLabels[mode]}</span>
                  <ChevronDownIcon className="size-3 opacity-60" />
                </Button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="start" className="w-44">
                <DropdownMenuLabel>Search target</DropdownMenuLabel>
                <DropdownMenuSeparator />
                {(["code", "discussions", "repositories"] as const).map(
                  (choice) => (
                    <DropdownMenuItem
                      key={choice}
                      onSelect={() => setMode(choice)}
                    >
                      {modeLabels[choice]}
                    </DropdownMenuItem>
                  ),
                )}
              </DropdownMenuContent>
            </DropdownMenu>

            {mode !== "repositories" && (
              <DropdownMenu modal={false}>
                <DropdownMenuTrigger asChild>
                  <Button
                    type="button"
                    variant="outline"
                    size="xs"
                    className="h-7 max-w-52 gap-1.5 bg-background/80 px-2.5 text-xs font-medium"
                  >
                    <RepoIcon size={13} aria-hidden="true" />
                    <span className="truncate">{scopeLabel}</span>
                    <ChevronDownIcon className="size-3 shrink-0 opacity-60" />
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent
                  align="start"
                  className="max-h-72 w-64 overflow-y-auto"
                >
                  <DropdownMenuLabel>Repository scope</DropdownMenuLabel>
                  <DropdownMenuSeparator />
                  <DropdownMenuItem onSelect={() => setSelectedRepo(undefined)}>
                    {organization ? organization.name : "All repositories"}
                  </DropdownMenuItem>
                  {repositories.length > 0 && <DropdownMenuSeparator />}
                  {repositories.slice(0, 15).map((repo) => (
                    <DropdownMenuItem
                      key={repo.id}
                      onSelect={() => setSelectedRepo(repo)}
                    >
                      <span className="truncate">
                        {repo.owner}/{repo.name}
                      </span>
                    </DropdownMenuItem>
                  ))}
                </DropdownMenuContent>
              </DropdownMenu>
            )}

            <Button
              type="button"
              variant="ghost"
              size="icon-xs"
              onClick={onNewRepository}
              aria-label="Create new repository"
              title="New repository"
              className="size-7 text-muted-foreground hover:text-foreground"
            >
              <PlusIcon className="size-3.5" aria-hidden="true" />
            </Button>
          </div>

          <div className="flex items-center gap-2">
            {mode === "code" && (
              <DropdownMenu modal={false}>
                <DropdownMenuTrigger asChild>
                  <Button
                    type="button"
                    variant="ghost"
                    size="xs"
                    className="h-7 gap-1 px-2 text-xs text-muted-foreground hover:text-foreground"
                  >
                    <span>Search in: {codeKindLabels[codeKind]}</span>
                    <ChevronDownIcon className="size-3 opacity-60" />
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end" className="w-44">
                  {(["any", "content", "path", "symbol"] as const).map(
                    (value) => (
                      <DropdownMenuItem
                        key={value}
                        onSelect={() => setCodeKind(value)}
                      >
                        {codeKindLabels[value]}
                      </DropdownMenuItem>
                    ),
                  )}
                </DropdownMenuContent>
              </DropdownMenu>
            )}

            <Button
              type="submit"
              variant="ghost"
              size="icon-xs"
              aria-label="Run workspace search"
              title="Run search"
              className="size-7 text-muted-foreground hover:text-foreground"
            >
              <SendHorizontalIcon className="size-4" aria-hidden="true" />
            </Button>
          </div>
        </div>
      </form>

      <div
        className="flex flex-wrap items-center justify-center gap-2"
        role="group"
        aria-label="Quick actions"
      >
        <Button
          asChild
          variant="outline"
          size="sm"
          className="h-8 rounded-lg bg-card px-3 text-xs font-medium"
        >
          <Link href="/-/code-search">
            <CodeIcon size={14} aria-hidden="true" />
            Search code
          </Link>
        </Button>
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={onNewRepository}
          className="h-8 rounded-lg bg-card px-3 text-xs font-medium"
        >
          <RepoIcon size={14} aria-hidden="true" />
          New repository
        </Button>
        <Button
          asChild
          variant="outline"
          size="sm"
          className="h-8 rounded-lg bg-card px-3 text-xs font-medium"
        >
          <Link href="/-/repositories">
            <RepoCloneIcon size={14} aria-hidden="true" />
            Import &amp; browse
          </Link>
        </Button>
        <Button
          asChild
          variant="outline"
          size="sm"
          className="h-8 rounded-lg bg-card px-3 text-xs font-medium"
        >
          <Link href="/-/work">
            <ListTodoIcon className="size-3.5" aria-hidden="true" />
            My work
          </Link>
        </Button>
        <Button
          asChild
          variant="outline"
          size="sm"
          className="h-8 rounded-lg bg-card px-3 text-xs font-medium"
        >
          <Link href="/-/organizations">
            <OrganizationIcon size={14} aria-hidden="true" />
            Organizations
          </Link>
        </Button>
      </div>
    </section>
  );
}

function HomeFeed({ repositories }: { repositories: Repository[] }) {
  const [filter, setFilter] = useState<"all" | "unread" | Reason>("all");
  const params = new URLSearchParams({
    unread: String(filter === "unread"),
    limit: "15",
  });
  if (filter !== "all" && filter !== "unread") {
    params.set("reason", filter);
  }
  const feed = useRequest<{ items: FeedNotice[]; next: string | null }>(
    `/api/notifications?${params}`,
  );

  const filterChips: { id: "all" | "unread" | Reason; label: string }[] = [
    { id: "all", label: "All activity" },
    { id: "unread", label: "Unread" },
    { id: "review_requested", label: "Review requests" },
    { id: "mention", label: "Mentions" },
    { id: "release", label: "Releases" },
  ];

  return (
    <div className="grid gap-6">
      <section aria-labelledby="home-feed-heading" className="grid gap-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h1
            id="home-feed-heading"
            className="m-0 text-xl font-semibold tracking-tight"
          >
            Feed
          </h1>
          <Link
            href="/-/notifications"
            className="inline-flex items-center gap-1 text-xs font-medium text-(--info) hover:underline"
          >
            <span>Open inbox</span>
            <ArrowRightIcon className="size-3" aria-hidden="true" />
          </Link>
        </div>

        <div
          className="flex flex-wrap gap-1.5"
          role="group"
          aria-label="Feed filters"
        >
          {filterChips.map((chip) => (
            <Button
              key={chip.id}
              type="button"
              size="xs"
              variant={filter === chip.id ? "secondary" : "outline"}
              onClick={() => setFilter(chip.id)}
              aria-pressed={filter === chip.id}
            >
              {chip.label}
            </Button>
          ))}
        </div>

        <div className="overflow-hidden rounded-lg border border-border bg-card">
          {feed.loading && !feed.data ? (
            <div
              role="status"
              className="flex items-center gap-2 px-4 py-6 text-sm text-muted-foreground"
            >
              <Spinner /> Loading feed…
            </div>
          ) : feed.error ? (
            <div
              role="alert"
              className="flex items-center justify-between gap-2 px-4 py-4 text-sm text-muted-foreground"
            >
              <span>{feed.error}</span>
              <Button variant="outline" size="xs" onClick={feed.retry}>
                Retry
              </Button>
            </div>
          ) : feed.data && feed.data.items.length > 0 ? (
            <ul
              className="m-0 list-none divide-y divide-border p-0"
              aria-label="Recent feed activity"
            >
              {feed.data.items.map((notice) => {
                const reasonMeta = reasons[notice.reason];
                const ReasonIcon = reasonMeta?.icon;
                return (
                  <li
                    key={notice.id}
                    className="flex items-start gap-3 px-4 py-3.5 hover:bg-muted/50"
                  >
                    <span
                      className={cn(
                        "mt-0.5 grid size-7 shrink-0 place-items-center rounded-full border border-border bg-muted text-muted-foreground",
                        !notice.read && "border-(--info)/40 text-(--info)",
                      )}
                      aria-hidden="true"
                    >
                      {ReasonIcon ? (
                        <ReasonIcon className="size-3.5" />
                      ) : (
                        <RepoIcon size={14} />
                      )}
                    </span>
                    <div className="min-w-0 flex-1">
                      <p className="m-0 text-xs text-muted-foreground">
                        <strong className="font-medium text-foreground">
                          {notice.sender}
                        </strong>{" "}
                        · {reasonMeta?.summary ?? "Update"} in{" "}
                        <Link
                          href={repoHref(notice.repository)}
                          className="font-medium text-foreground hover:text-(--info) hover:underline"
                        >
                          {notice.repository.owner}/{notice.repository.name}
                        </Link>{" "}
                        · <RelativeTime value={notice.updated.created_at_ms} />
                      </p>
                      <Link
                        href={notice.href}
                        className="mt-1 block text-sm font-semibold text-foreground hover:text-(--info) hover:underline"
                      >
                        {notice.title}
                      </Link>
                    </div>
                    {!notice.read && (
                      <Badge
                        variant="secondary"
                        className="shrink-0 text-[10px]"
                      >
                        Unread
                      </Badge>
                    )}
                  </li>
                );
              })}
            </ul>
          ) : (
            <div className="px-4 py-6 text-sm text-muted-foreground">
              No feed updates yet. Watch repositories or participate in issues
              and pull requests to populate your feed.
            </div>
          )}
        </div>
      </section>

      {repositories.length > 0 && (
        <section
          aria-labelledby="home-feed-repos-heading"
          className="grid gap-2.5"
        >
          <div className="flex items-center justify-between gap-2">
            <h2
              id="home-feed-repos-heading"
              className="m-0 text-sm font-semibold"
            >
              Active repositories
            </h2>
            <Link
              href="/-/repositories"
              className="text-xs font-medium text-(--info) hover:underline"
            >
              View all
            </Link>
          </div>
          <ul className="m-0 grid list-none gap-2.5 p-0 sm:grid-cols-2">
            {repositories.slice(0, 6).map((repo) => (
              <li
                key={repo.id}
                className="flex flex-col justify-between gap-2 rounded-lg border border-border bg-card p-3.5 transition-colors hover:border-input-hover"
              >
                <div className="min-w-0">
                  <div className="flex items-center gap-1.5">
                    {repo.fork ? (
                      <RepoForkedIcon
                        size={14}
                        className="shrink-0 text-muted-foreground"
                        aria-hidden="true"
                      />
                    ) : (
                      <RepoIcon
                        size={14}
                        className="shrink-0 text-muted-foreground"
                        aria-hidden="true"
                      />
                    )}
                    <Link
                      href={repoHref(repo)}
                      className="truncate text-sm font-semibold text-foreground hover:text-(--info) hover:underline"
                    >
                      {repo.owner}/{repo.name}
                    </Link>
                  </div>
                  {repo.description && (
                    <p
                      dir="auto"
                      className="m-0 mt-1 line-clamp-2 text-xs text-muted-foreground"
                    >
                      {repo.description}
                    </p>
                  )}
                </div>
                <div className="flex items-center gap-2">
                  {repo.visibility && (
                    <Badge variant="outline" className="text-[10px]">
                      {repo.visibility === "public" ? "Public" : "Private"}
                    </Badge>
                  )}
                  {repo.archived && (
                    <Badge variant="outline" className="text-[10px]">
                      Archived
                    </Badge>
                  )}
                </div>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
