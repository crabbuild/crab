import {
  CodeIcon,
  GitPullRequestIcon,
  GlobeIcon,
  HomeIcon,
  IssueOpenedIcon,
  KeyIcon,
  LockIcon,
  OrganizationIcon,
  PlusIcon,
  RepoCloneIcon,
  RepoIcon,
  SearchIcon,
  TriangleDownIcon,
} from "@primer/octicons-react";
import {
  Building2Icon,
  EyeIcon,
  ListTodoIcon,
  LogOutIcon,
  MenuIcon,
  MonitorIcon,
  MoonIcon,
  PaintbrushIcon,
  SettingsIcon,
  SmileIcon,
  SunIcon,
  UserRoundIcon,
  XIcon,
} from "lucide-react";
import { useEffect, useRef, useState, type FormEvent } from "react";
import { AppearanceControl, type Theme } from "@/app/theme/control";
import { Link } from "@/components/navigation/link";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { UserAvatar } from "@/components/ui/user-avatar";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import { Spinner } from "@/components/ui/spinner";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { signInHref } from "@/features/auth/sign-in";
import { NotificationLink } from "@/features/notifications";
import { navigate, useRequest, type Repository } from "@/lib/api";
import type { AvatarConfig, Organization, Session } from "@/lib/contracts";
import { organizationHref } from "@/lib/organization-route";
import { repoHref } from "@/lib/repository-route";
import { cn } from "@/lib/utils";
import crabMark from "../../../web/crab.optimized.svg";

function isEditableElement(target: EventTarget | null) {
  if (!(target instanceof HTMLElement)) return false;
  if (target.isContentEditable) return true;
  const tag = target.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT";
}

export function GlobalHeader({
  session,
  avatar,
  url,
  repo,
  repositoryOrganization,
  contextLabel,
  contextHref,
  notificationsActive,
  theme,
  setTheme,
  signingOut,
  onSignOut,
  catalogRepositories,
}: {
  session: Session;
  avatar?: AvatarConfig | null;
  url: URL;
  repo?: Repository;
  repositoryOrganization?: Organization;
  contextLabel: string;
  contextHref: string;
  notificationsActive: boolean;
  theme: Theme;
  setTheme: (theme: Theme) => void;
  signingOut: boolean;
  onSignOut: () => void;
  catalogRepositories: Repository[];
}) {
  const [drawerOpen, setDrawerOpen] = useState(false);
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchText, setSearchText] = useState("");
  const searchInputRef = useRef<HTMLInputElement>(null);

  const authenticated = session.authenticated;
  const userName = session.user?.name ?? "Local session";

  // Only load repository suggestions when the drawer or search palette is opened.
  const navCatalog = useRequest<{ repositories: Repository[] }>(
    authenticated && (drawerOpen || searchOpen) ? "/api/repos?limit=32" : null,
  );
  const quickRepos =
    navCatalog.data?.repositories ??
    (catalogRepositories.length ? catalogRepositories : []);

  useEffect(() => {
    if (!authenticated) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (
        event.key === "/" &&
        !event.metaKey &&
        !event.ctrlKey &&
        !event.altKey &&
        !isEditableElement(event.target)
      ) {
        event.preventDefault();
        setSearchOpen(true);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [authenticated]);

  useEffect(() => {
    if (searchOpen) {
      requestAnimationFrame(() => searchInputRef.current?.focus());
    }
  }, [searchOpen]);

  useEffect(() => {
    if (!drawerOpen) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") setDrawerOpen(false);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [drawerOpen]);

  const trimmedSearch = searchText.trim();
  const matchingRepos = trimmedSearch
    ? quickRepos
        .filter((entry) =>
          `${entry.owner}/${entry.name}`
            .toLowerCase()
            .includes(trimmedSearch.toLowerCase()),
        )
        .slice(0, 5)
    : quickRepos.slice(0, 5);

  const submitCommandSearch = (event: FormEvent) => {
    event.preventDefault();
    const target = trimmedSearch
      ? `/-/code-search?${new URLSearchParams({ q: trimmedSearch })}`
      : "/-/code-search";
    setSearchOpen(false);
    setSearchText("");
    navigate(target);
  };

  const navItems = [
    {
      label: "Home",
      href: "/",
      Icon: HomeIcon,
      active: url.pathname === "/" && url.searchParams.get("tab") !== "feed",
    },
    {
      label: "Feed",
      href: "/?tab=feed",
      Icon: GlobeIcon,
      active: url.pathname === "/" && url.searchParams.get("tab") === "feed",
    },
    {
      label: "Issues",
      href: "/-/work?kind=issue",
      Icon: IssueOpenedIcon,
      active:
        url.pathname === "/-/work" && url.searchParams.get("kind") === "issue",
    },
    {
      label: "Pull requests",
      href: "/-/work?kind=pull",
      Icon: GitPullRequestIcon,
      active:
        url.pathname === "/-/work" && url.searchParams.get("kind") !== "issue",
    },
    {
      label: "Repositories",
      href: "/-/repositories",
      Icon: RepoIcon,
      active: url.pathname === "/-/repositories",
    },
    {
      label: "Organizations",
      href: "/-/organizations",
      Icon: OrganizationIcon,
      active: url.pathname === "/-/organizations",
    },
    {
      label: "Search code",
      href: "/-/code-search",
      Icon: CodeIcon,
      active: url.pathname === "/-/code-search",
    },
    {
      label: "Search issues and PRs",
      href: "/-/discussion-search",
      Icon: SearchIcon,
      active: url.pathname === "/-/discussion-search",
    },
    {
      label: "Settings",
      href: "/-/settings",
      Icon: SettingsIcon,
      active:
        url.pathname === "/-/profile" || url.pathname.startsWith("/-/settings"),
    },
  ];

  return (
    <>
      <header className="global-header">
        <div className="flex min-w-0 items-center gap-2.5">
          {authenticated && (
            <Button
              type="button"
              variant="outline"
              size="icon-sm"
              aria-label="Open navigation menu"
              aria-expanded={drawerOpen}
              onClick={() => setDrawerOpen(true)}
              className="shrink-0 bg-background/60 text-muted-foreground hover:text-foreground"
            >
              <MenuIcon className="size-4" aria-hidden="true" />
            </Button>
          )}
          <Link
            className="brand shrink-0"
            href="/"
            aria-label="Crab repositories"
          >
            <img className="brand-mark" src={crabMark} alt="" />
          </Link>
          {repo ? (
            <div className="global-repository">
              <RepoIcon size={16} />
              <h1 className="wrap-anywhere">
                {repositoryOrganization ? (
                  <>
                    <Link
                      href={organizationHref(
                        repositoryOrganization,
                        "?tab=repositories",
                      )}
                    >
                      <bdi>{repositoryOrganization.name}</bdi>
                    </Link>{" "}
                    <span aria-hidden="true">/</span>{" "}
                    <Link href={repoHref(repo)}>
                      <strong>{repo.name}</strong>
                    </Link>
                  </>
                ) : (
                  <Link href={repoHref(repo)}>
                    {repo.owner} <span aria-hidden="true">/</span>{" "}
                    <strong>{repo.name}</strong>
                  </Link>
                )}
              </h1>
              {repo.archived && <Badge variant="outline">Archived</Badge>}
              {repo.visibility === "public" && (
                <Badge variant="outline">Public</Badge>
              )}
            </div>
          ) : (
            <Link
              className="global-catalog-link text-sm font-semibold text-foreground hover:no-underline"
              href={contextHref}
            >
              {contextLabel}
            </Link>
          )}
        </div>

        <div className="header-actions">
          {authenticated && (
            <button
              type="button"
              onClick={() => setSearchOpen(true)}
              aria-label="Search Crab"
              className="hidden h-8 w-44 items-center justify-start gap-2 rounded-md border border-border bg-background/80 px-2.5 text-left text-xs text-muted-foreground transition-colors hover:border-input-hover hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none md:inline-flex lg:w-52"
            >
              <SearchIcon size={14} className="shrink-0" aria-hidden="true" />
              <span className="truncate">
                Type{" "}
                <kbd className="mx-0.5 inline-flex h-4 min-w-4 items-center justify-center rounded border border-border bg-muted px-1 font-mono text-[10px] leading-none text-foreground">
                  /
                </kbd>{" "}
                to search
              </span>
            </button>
          )}

          {authenticated && (
            <Button
              type="button"
              variant="outline"
              size="icon-sm"
              onClick={() => setSearchOpen(true)}
              aria-label="Search Crab"
              className="bg-background/60 text-muted-foreground hover:text-foreground md:hidden"
            >
              <SearchIcon size={15} aria-hidden="true" />
            </Button>
          )}

          {authenticated && (
            <span
              className="hidden h-4 w-px bg-border sm:block"
              aria-hidden="true"
            />
          )}

          {authenticated && (
            <DropdownMenu modal={false}>
              <DropdownMenuTrigger asChild>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  aria-label="Create new"
                  title="Create new…"
                  className="h-8 gap-0.5 bg-background/60 px-2 text-muted-foreground hover:text-foreground"
                >
                  <PlusIcon size={14} aria-hidden="true" />
                  <TriangleDownIcon size={14} aria-hidden="true" />
                </Button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end" className="w-52 p-1.5">
                <DropdownMenuItem
                  asChild
                  className="h-8.5 gap-2 rounded-md px-2"
                >
                  <Link href="/-/new">
                    <RepoIcon size={15} aria-hidden="true" />
                    New repository
                  </Link>
                </DropdownMenuItem>
                <DropdownMenuItem
                  asChild
                  className="h-8.5 gap-2 rounded-md px-2"
                >
                  <Link href="/-/repositories">
                    <RepoCloneIcon size={15} aria-hidden="true" />
                    Import repository
                  </Link>
                </DropdownMenuItem>
                <DropdownMenuSeparator className="my-1" />
                <DropdownMenuItem
                  asChild
                  className="h-8.5 gap-2 rounded-md px-2"
                >
                  <Link href="/-/organizations">
                    <Building2Icon aria-hidden="true" />
                    Your organizations
                  </Link>
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
          )}

          {authenticated && (
            <div className="hidden items-center gap-1.5 sm:flex">
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    asChild
                    variant="outline"
                    size="icon-sm"
                    className="bg-background/60 text-muted-foreground hover:text-foreground"
                  >
                    <Link
                      href="/-/work?kind=issue"
                      aria-label="Issues"
                      aria-current={
                        url.pathname === "/-/work" &&
                        url.searchParams.get("kind") === "issue"
                          ? "page"
                          : undefined
                      }
                    >
                      <IssueOpenedIcon size={15} aria-hidden="true" />
                    </Link>
                  </Button>
                </TooltipTrigger>
                <TooltipContent>Issues</TooltipContent>
              </Tooltip>

              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    asChild
                    variant="outline"
                    size="icon-sm"
                    className="bg-background/60 text-muted-foreground hover:text-foreground"
                  >
                    <Link
                      href="/-/work?kind=pull"
                      aria-label="Pull requests"
                      aria-current={
                        url.pathname === "/-/work" &&
                        url.searchParams.get("kind") !== "issue"
                          ? "page"
                          : undefined
                      }
                    >
                      <GitPullRequestIcon size={15} aria-hidden="true" />
                    </Link>
                  </Button>
                </TooltipTrigger>
                <TooltipContent>Pull requests</TooltipContent>
              </Tooltip>

              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    asChild
                    variant="outline"
                    size="icon-sm"
                    className="bg-background/60 text-muted-foreground hover:text-foreground"
                  >
                    <Link
                      href="/-/repositories"
                      aria-label="Repositories"
                      aria-current={
                        url.pathname === "/-/repositories" ? "page" : undefined
                      }
                    >
                      <RepoIcon size={15} aria-hidden="true" />
                    </Link>
                  </Button>
                </TooltipTrigger>
                <TooltipContent>Repositories</TooltipContent>
              </Tooltip>
            </div>
          )}

          {!authenticated && (
            <>
              <AppearanceControl theme={theme} setTheme={setTheme} />
              <Button asChild variant="outline" size="sm">
                <a href={signInHref(url)}>Sign in</a>
              </Button>
            </>
          )}

          {session.user && <NotificationLink current={notificationsActive} />}

          {authenticated && (
            <DropdownMenu modal={false}>
              <DropdownMenuTrigger asChild>
                <Button
                  variant="ghost"
                  size="icon-sm"
                  className="size-8 rounded-full border border-border p-0 hover:ring-2 hover:ring-ring/25"
                  aria-label={signingOut ? "Signing out…" : "Open user menu"}
                  title={userName}
                  disabled={signingOut}
                >
                  {signingOut ? (
                    <Spinner />
                  ) : (
                    <UserAvatar
                      name={userName}
                      avatar={avatar}
                      size={32}
                      className="border-0"
                    />
                  )}
                </Button>
              </DropdownMenuTrigger>
              <DropdownMenuContent
                align="end"
                sideOffset={8}
                collisionPadding={16}
                className="w-64 max-w-[calc(100vw-2rem)] rounded-xl border border-border bg-popover p-1.5 shadow-lg"
              >
                <DropdownMenuLabel className="p-0 font-normal">
                  <div className="flex items-center gap-2.5 px-2.5 py-2">
                    <UserAvatar name={userName} avatar={avatar} size={36} />
                    <div className="min-w-0 flex-1">
                      {session.user ? (
                        <>
                          <span className="block truncate text-sm leading-tight font-semibold text-foreground">
                            {session.user.name}
                          </span>
                          <span className="block truncate text-[11px] text-muted-foreground">
                            Signed in as
                          </span>
                        </>
                      ) : (
                        <span className="block truncate text-sm font-semibold text-foreground">
                          Local session
                        </span>
                      )}
                    </div>
                  </div>
                </DropdownMenuLabel>

                <DropdownMenuSeparator className="my-1" />

                <DropdownMenuGroup className="grid gap-0.5">
                  <DropdownMenuItem
                    asChild
                    className="h-9 gap-2.5 rounded-lg px-2.5 text-sm"
                  >
                    <Link href="/-/profile">
                      <UserRoundIcon aria-hidden="true" />
                      Your profile
                    </Link>
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    asChild
                    className="h-9 gap-2.5 rounded-lg px-2.5 text-sm"
                  >
                    <Link href="/-/settings/review-availability">
                      <SmileIcon aria-hidden="true" />
                      Set status
                    </Link>
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    asChild
                    className="h-9 gap-2.5 rounded-lg px-2.5 text-sm"
                  >
                    <Link href="/-/repositories/personal">
                      <RepoIcon aria-hidden="true" />
                      Your repositories
                    </Link>
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    asChild
                    className="h-9 gap-2.5 rounded-lg px-2.5 text-sm"
                  >
                    <Link href="/-/organizations">
                      <Building2Icon aria-hidden="true" />
                      Your organizations
                    </Link>
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    asChild
                    className="h-9 gap-2.5 rounded-lg px-2.5 text-sm"
                  >
                    <Link href="/-/work">
                      <ListTodoIcon aria-hidden="true" />
                      My work
                    </Link>
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    asChild
                    className="h-9 gap-2.5 rounded-lg px-2.5 text-sm"
                  >
                    <Link href="/-/notifications/watched">
                      <EyeIcon aria-hidden="true" />
                      Watched repositories
                    </Link>
                  </DropdownMenuItem>
                </DropdownMenuGroup>

                <DropdownMenuSeparator className="my-1" />

                <DropdownMenuGroup className="grid gap-0.5">
                  <DropdownMenuItem
                    asChild
                    className="h-9 gap-2.5 rounded-lg px-2.5 text-sm"
                  >
                    <Link href="/-/settings">
                      <SettingsIcon aria-hidden="true" />
                      Settings
                    </Link>
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    asChild
                    className="h-9 gap-2.5 rounded-lg px-2.5 text-sm"
                  >
                    <Link href="/-/settings/appearance">
                      <PaintbrushIcon aria-hidden="true" />
                      Appearance
                    </Link>
                  </DropdownMenuItem>
                  {session.user && (
                    <DropdownMenuItem
                      asChild
                      className="h-9 gap-2.5 rounded-lg px-2.5 text-sm"
                    >
                      <Link href="/-/settings/git-access">
                        <KeyIcon aria-hidden="true" />
                        Git access
                      </Link>
                    </DropdownMenuItem>
                  )}
                </DropdownMenuGroup>

                <DropdownMenuSeparator className="my-1" />

                <DropdownMenuLabel className="px-2.5 pt-1 pb-1.5 text-[11px] font-medium text-muted-foreground">
                  Theme
                </DropdownMenuLabel>
                <DropdownMenuRadioGroup
                  value={theme}
                  onValueChange={(value) => {
                    if (
                      value === "auto" ||
                      value === "light" ||
                      value === "dark"
                    ) {
                      setTheme(value);
                    }
                  }}
                  className="grid grid-cols-3 gap-1 rounded-lg bg-muted/50 p-1"
                >
                  <DropdownMenuRadioItem
                    value="auto"
                    className="h-7.5 justify-center gap-1 rounded-md px-1.5 py-0 text-xs font-medium text-muted-foreground data-[state=checked]:bg-background data-[state=checked]:text-foreground data-[state=checked]:shadow-xs [&_[data-slot=dropdown-menu-radio-item-indicator]]:hidden"
                  >
                    <MonitorIcon className="size-3.5" aria-hidden="true" />
                    System
                  </DropdownMenuRadioItem>
                  <DropdownMenuRadioItem
                    value="light"
                    className="h-7.5 justify-center gap-1 rounded-md px-1.5 py-0 text-xs font-medium text-muted-foreground data-[state=checked]:bg-background data-[state=checked]:text-foreground data-[state=checked]:shadow-xs [&_[data-slot=dropdown-menu-radio-item-indicator]]:hidden"
                  >
                    <SunIcon className="size-3.5" aria-hidden="true" />
                    Light
                  </DropdownMenuRadioItem>
                  <DropdownMenuRadioItem
                    value="dark"
                    className="h-7.5 justify-center gap-1 rounded-md px-1.5 py-0 text-xs font-medium text-muted-foreground data-[state=checked]:bg-background data-[state=checked]:text-foreground data-[state=checked]:shadow-xs [&_[data-slot=dropdown-menu-radio-item-indicator]]:hidden"
                  >
                    <MoonIcon className="size-3.5" aria-hidden="true" />
                    Dark
                  </DropdownMenuRadioItem>
                </DropdownMenuRadioGroup>

                {session.user && (
                  <>
                    <DropdownMenuSeparator className="my-1" />
                    <DropdownMenuGroup>
                      <DropdownMenuItem
                        variant="destructive"
                        disabled={signingOut}
                        onSelect={onSignOut}
                        className="h-9 gap-2.5 rounded-lg px-2.5 text-sm"
                      >
                        <LogOutIcon aria-hidden="true" />
                        Sign out
                      </DropdownMenuItem>
                    </DropdownMenuGroup>
                  </>
                )}
              </DropdownMenuContent>
            </DropdownMenu>
          )}
        </div>
      </header>

      {drawerOpen && (
        <div
          className="fixed inset-0 z-50 flex"
          role="dialog"
          aria-modal="true"
          aria-label="Global navigation"
        >
          <div
            className="fixed inset-0 bg-overlay backdrop-blur-xs"
            onClick={() => setDrawerOpen(false)}
          />
          <nav className="relative z-10 flex h-full w-80 max-w-[85vw] flex-col overflow-y-auto border-r border-border bg-popover p-4 text-popover-foreground shadow-xl">
            <div className="mb-3 flex items-center justify-between gap-2">
              <Link
                href="/"
                onClick={() => setDrawerOpen(false)}
                className="flex items-center gap-2 font-semibold text-foreground hover:no-underline"
              >
                <img src={crabMark} alt="" className="size-6 object-contain" />
                <span>Crab</span>
              </Link>
              <Button
                type="button"
                variant="ghost"
                size="icon-sm"
                aria-label="Close navigation menu"
                onClick={() => setDrawerOpen(false)}
              >
                <XIcon className="size-4" aria-hidden="true" />
              </Button>
            </div>

            <ul className="m-0 grid list-none gap-0.5 p-0">
              {navItems.map(({ label, href, Icon, active }) => (
                <li key={href}>
                  <Link
                    href={href}
                    onClick={() => setDrawerOpen(false)}
                    aria-current={active ? "page" : undefined}
                    className={cn(
                      "flex items-center gap-2.5 rounded-md px-2.5 py-2 text-sm hover:bg-accent/70 hover:no-underline",
                      active
                        ? "bg-accent font-medium text-foreground"
                        : "text-foreground/90",
                    )}
                  >
                    <Icon
                      size={16}
                      className="shrink-0 text-muted-foreground"
                      aria-hidden="true"
                    />
                    <span>{label}</span>
                  </Link>
                </li>
              ))}
            </ul>

            <Separator className="my-3" />

            <div className="mb-2 flex items-center justify-between px-2">
              <span className="text-xs font-semibold text-muted-foreground">
                Repositories
              </span>
              <Link
                href="/-/repositories"
                onClick={() => setDrawerOpen(false)}
                className="text-xs text-(--info) hover:underline"
              >
                View all
              </Link>
            </div>
            {navCatalog.loading && quickRepos.length === 0 ? (
              <p className="flex items-center gap-2 px-2 py-2 text-xs text-muted-foreground">
                <Spinner /> Loading…
              </p>
            ) : quickRepos.length > 0 ? (
              <ul className="m-0 grid list-none gap-0.5 p-0">
                {quickRepos.slice(0, 8).map((entry) => (
                  <li key={entry.id}>
                    <Link
                      href={repoHref(entry)}
                      onClick={() => setDrawerOpen(false)}
                      className="flex items-center gap-2 rounded-md px-2.5 py-1.5 text-sm text-foreground hover:bg-accent/70 hover:no-underline"
                    >
                      <RepoIcon
                        size={14}
                        className="shrink-0 text-muted-foreground"
                        aria-hidden="true"
                      />
                      <span className="min-w-0 flex-1 truncate text-xs font-medium">
                        {entry.owner}/{entry.name}
                      </span>
                      {entry.visibility === "private" && (
                        <LockIcon
                          size={12}
                          className="shrink-0 text-muted-foreground/70"
                          aria-hidden="true"
                        />
                      )}
                    </Link>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="m-0 px-2 text-xs text-muted-foreground">
                No repositories yet.
              </p>
            )}
          </nav>
        </div>
      )}

      <Dialog open={searchOpen} onOpenChange={setSearchOpen}>
        <DialogContent className="gap-0 overflow-hidden p-0 sm:max-w-xl">
          <DialogHeader className="sr-only">
            <DialogTitle>Search Crab</DialogTitle>
            <DialogDescription>
              Jump to a repository or search code, issues, and pull requests.
            </DialogDescription>
          </DialogHeader>
          <form
            onSubmit={submitCommandSearch}
            className="border-b border-border p-3"
          >
            <div className="flex items-center gap-2">
              <SearchIcon
                size={16}
                className="shrink-0 text-muted-foreground"
                aria-hidden="true"
              />
              <Input
                ref={searchInputRef}
                type="search"
                value={searchText}
                onChange={(event) => setSearchText(event.target.value)}
                placeholder="Search code, issues, PRs, or jump to a repository…"
                aria-label="Search Crab"
                className="border-0 bg-transparent shadow-none focus-visible:border-transparent focus-visible:ring-0"
              />
            </div>
          </form>

          <div className="max-h-96 overflow-y-auto p-2">
            <div className="grid gap-0.5">
              <button
                type="button"
                onClick={() => {
                  const destination = trimmedSearch
                    ? `/-/code-search?${new URLSearchParams({ q: trimmedSearch })}`
                    : "/-/code-search";
                  setSearchOpen(false);
                  setSearchText("");
                  navigate(destination);
                }}
                className="flex items-center gap-2.5 rounded-md px-2.5 py-2 text-left text-sm hover:bg-accent"
              >
                <CodeIcon
                  size={15}
                  className="shrink-0 text-muted-foreground"
                  aria-hidden="true"
                />
                <span className="min-w-0 flex-1 truncate">
                  {trimmedSearch ? (
                    <>
                      Search code for <strong>{trimmedSearch}</strong>
                    </>
                  ) : (
                    "Search code across repositories"
                  )}
                </span>
                <span className="text-xs text-muted-foreground">Code</span>
              </button>

              <button
                type="button"
                onClick={() => {
                  const destination = trimmedSearch
                    ? `/-/discussion-search?${new URLSearchParams({ q: trimmedSearch })}`
                    : "/-/discussion-search";
                  setSearchOpen(false);
                  setSearchText("");
                  navigate(destination);
                }}
                className="flex items-center gap-2.5 rounded-md px-2.5 py-2 text-left text-sm hover:bg-accent"
              >
                <GitPullRequestIcon
                  size={15}
                  className="shrink-0 text-muted-foreground"
                  aria-hidden="true"
                />
                <span className="min-w-0 flex-1 truncate">
                  {trimmedSearch ? (
                    <>
                      Search issues &amp; PRs for{" "}
                      <strong>{trimmedSearch}</strong>
                    </>
                  ) : (
                    "Search issues and pull requests"
                  )}
                </span>
                <span className="text-xs text-muted-foreground">
                  Discussions
                </span>
              </button>

              <button
                type="button"
                onClick={() => {
                  const destination = trimmedSearch
                    ? `/-/repositories?${new URLSearchParams({ q: trimmedSearch })}`
                    : "/-/repositories";
                  setSearchOpen(false);
                  setSearchText("");
                  navigate(destination);
                }}
                className="flex items-center gap-2.5 rounded-md px-2.5 py-2 text-left text-sm hover:bg-accent"
              >
                <RepoIcon
                  size={15}
                  className="shrink-0 text-muted-foreground"
                  aria-hidden="true"
                />
                <span className="min-w-0 flex-1 truncate">
                  {trimmedSearch ? (
                    <>
                      Search repositories for <strong>{trimmedSearch}</strong>
                    </>
                  ) : (
                    "Browse all accessible repositories"
                  )}
                </span>
                <span className="text-xs text-muted-foreground">
                  Repositories
                </span>
              </button>
            </div>

            {matchingRepos.length > 0 && (
              <>
                <Separator className="my-2" />
                <p className="m-0 px-2.5 py-1 text-xs font-semibold text-muted-foreground">
                  Repositories
                </p>
                <ul className="m-0 grid list-none gap-0.5 p-0">
                  {matchingRepos.map((entry) => (
                    <li key={entry.id}>
                      <Link
                        href={repoHref(entry)}
                        onClick={() => {
                          setSearchOpen(false);
                          setSearchText("");
                        }}
                        className="flex items-center gap-2.5 rounded-md px-2.5 py-1.5 text-sm text-foreground hover:bg-accent hover:no-underline"
                      >
                        <RepoIcon
                          size={14}
                          className="shrink-0 text-muted-foreground"
                          aria-hidden="true"
                        />
                        <span className="min-w-0 flex-1 truncate font-medium">
                          {entry.owner}/{entry.name}
                        </span>
                        <span className="text-xs text-muted-foreground">
                          Jump to
                        </span>
                      </Link>
                    </li>
                  ))}
                </ul>
              </>
            )}
          </div>
        </DialogContent>
      </Dialog>
    </>
  );
}
