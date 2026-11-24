import { ProjectCommandsProvider } from "@/features/projects/commands";
import { OrganizationProjects } from "@/features/projects/workspace";
import {
  AppearanceControl,
  ThemeControl,
  type Theme,
} from "@/app/theme/control";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Spinner } from "@/components/ui/spinner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { SignIn } from "@/features/auth/sign-in";
import { GitAccess } from "@/features/git-access";
import { GitImport } from "@/features/imports";
import { CreateRepository } from "@/features/repository/create";
import { NotificationLink, Notifications } from "@/features/notifications";
import { DiscussionSubscriptions } from "@/features/notifications/subscriptions";
import { WatchedRepositories } from "@/features/notifications/watched";
import { gitOwnerSuggestion } from "@/features/imports/source";
import {
  OrganizationPage,
  OrganizationInvitation,
} from "@/features/organizations";
import {
  TeamFeedback,
  useTeamCommands,
} from "@/features/organizations/teams/commands";
import { ProfileSettings, UserProfile } from "@/features/profile";
import { LogOutIcon, UserRoundIcon } from "lucide-react";
import { RepositoryPage } from "@/features/repository/page";
import { TransferPage } from "@/features/settings/transfer";
import { BrowseRepositories } from "@/features/repository/catalog-browse";
import {
  DiscussionCodeTheme,
  DiscussionCsrf,
  DiscussionRepository,
} from "@/components/discussion/repository-context";
import type { Session } from "@/lib/contracts";
import { navigate, useLocation, useRequest, type Repository } from "@/lib/api";
import {
  codeThemeChoices,
  codeThemeFrom,
  codeThemeNamesFor,
  type CodeTheme,
} from "@/lib/code-theme";
import { repoHref, repositoryRoute } from "@/lib/repository-route";
import { RepoIcon, SearchIcon } from "@primer/octicons-react";
import { lazy, Suspense, useEffect, useLayoutEffect, useState } from "react";
import crabMark from "../../../web/crab.optimized.svg";

const WorkspaceCodeSearch = lazy(
  () => import("@/features/repository/workspace-code-search"),
);

export function App() {
  const location = useLocation();
  const [theme, setTheme] = useState<Theme>(() => {
    const saved = localStorage.getItem("crab-theme");
    return saved === "light" || saved === "dark" ? saved : "auto";
  });
  const [codeTheme, setCodeTheme] = useState<CodeTheme>(() =>
    codeThemeFrom(localStorage.getItem("crab-code-theme")),
  );
  const [systemDark, setSystemDark] = useState(
    () => matchMedia("(prefers-color-scheme: dark)").matches,
  );
  useEffect(() => {
    const media = matchMedia("(prefers-color-scheme: dark)");
    const update = () => setSystemDark(media.matches);
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, []);
  useEffect(() => {
    localStorage.setItem("crab-theme", theme);
  }, [theme]);
  useEffect(() => {
    localStorage.setItem("crab-code-theme", codeTheme);
  }, [codeTheme]);
  useEffect(() => {
    void import("@pierre/diffs").then(({ preloadHighlighter }) =>
      preloadHighlighter({
        themes: codeThemeNamesFor(codeTheme),
        langs: ["text"],
      }),
    );
  }, [codeTheme]);
  const selectCodeTheme = (next: CodeTheme) => {
    void import("@pierre/diffs")
      .then(({ preloadHighlighter }) =>
        preloadHighlighter({
          themes: codeThemeNamesFor(next),
          langs: ["text"],
        }),
      )
      .then(() => setCodeTheme(next));
  };
  const resolved = theme === "auto" ? (systemDark ? "dark" : "light") : theme;
  useLayoutEffect(() => {
    const root = document.documentElement;
    root.style.colorScheme = resolved;
    root.classList.toggle("dark", resolved === "dark");
    const browserTheme = document.querySelector<HTMLMetaElement>(
      'meta[name="theme-color"]',
    );
    if (browserTheme)
      browserTheme.content = getComputedStyle(document.body).backgroundColor;
    root.dataset.codeTheme = codeTheme;
    root.dataset.themeChanging = "";
    let settledFrame: number | undefined;
    const paintedFrame = requestAnimationFrame(() => {
      settledFrame = requestAnimationFrame(() => {
        delete root.dataset.themeChanging;
      });
    });
    return () => {
      cancelAnimationFrame(paintedFrame);
      if (settledFrame !== undefined) cancelAnimationFrame(settledFrame);
      delete root.dataset.themeChanging;
    };
  }, [codeTheme, resolved]);
  const url = new URL(location, window.location.origin);
  const codeSearch = url.pathname === "/-/code-search";
  const inbox = url.pathname === "/-/notifications";
  const watched = url.pathname === "/-/notifications/watched";
  const subscriptions = url.pathname === "/-/notifications/subscriptions";
  const notifications = inbox || watched || subscriptions;
  const savedReplies = url.pathname === "/-/settings/saved-replies";
  const reviewAvailability = url.pathname === "/-/settings/review-availability";
  const profileSettings =
    url.pathname === "/-/profile" || savedReplies || reviewAvailability;
  const actorId =
    /^\/-\/users\/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$/i.exec(
      url.pathname,
    )?.[1];
  const transferId = /^\/-\/repository-transfers\/([0-9a-f-]{36})$/i.exec(
    url.pathname,
  )?.[1];
  const organizationRoute =
    /^\/-\/organizations\/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})(\/invitation|\/projects(?:\/([0-9a-f-]{36}))?)?$/i.exec(
      url.pathname,
    );
  const route =
    profileSettings ||
    actorId ||
    transferId ||
    organizationRoute ||
    notifications ||
    codeSearch
      ? undefined
      : repositoryRoute(url);
  const projectWorkspace = organizationRoute?.[2]?.startsWith("/projects")
    ? organizationRoute[1]
    : undefined;
  const session = useRequest<Session>("/api/session");
  const authenticated = session.data?.authenticated;
  useLayoutEffect(() => {
    if (document.activeElement === document.body)
      document.getElementById("main")?.focus();
  }, [location, authenticated]);
  const teamAccount = !session.data?.authenticated
    ? null
    : session.data.mode === "local"
      ? "local"
      : session.data.user
        ? JSON.stringify([session.data.user.issuer, session.data.user.subject])
        : null;
  const importAccount = teamAccount ?? "local";
  const teamCommands = useTeamCommands(session.data?.csrf ?? "", teamAccount);
  const [signingOut, setSigningOut] = useState(false);
  const [sessionError, setSessionError] = useState<string>();
  const userInitial = Array.from(
    session.data?.user?.name.trim() ?? "",
  )[0]?.toUpperCase();
  useEffect(() => {
    const expired = () => session.retry();
    window.addEventListener("crab-session-expired", expired);
    return () => window.removeEventListener("crab-session-expired", expired);
  });
  const [gitAccessOpen, setGitAccessOpen] = useState(false);
  const catalog = useRequest<{ repositories: Repository[] }>(
    session.data?.authenticated && (inbox || subscriptions || gitAccessOpen)
      ? "/api/repos"
      : null,
  );
  const [importedRepository, setImportedRepository] =
    useState<Pick<Repository, "owner" | "name">>();
  const requestedRepository = route ?? importedRepository;
  const repositoryCatalog = useRequest<{
    repositories: Repository[];
    redirect?: Pick<Repository, "id" | "owner" | "name">;
  }>(
    session.data?.authenticated && requestedRepository
      ? `/api/repos?${new URLSearchParams({ owner: requestedRepository.owner, name: requestedRepository.name })}`
      : null,
  );
  async function signOut() {
    setSigningOut(true);
    setSessionError(undefined);
    try {
      const response = await fetch("/auth/logout", {
        method: "POST",
        headers: { "X-CSRF-Token": session.data?.csrf ?? "" },
      });
      if (!response.ok)
        throw new Error("Sign-out failed. Reload and try again.");
      // A full reload disposes all repository content held by the previous session.
      window.location.assign("/");
    } catch (error) {
      setSessionError(
        error instanceof Error ? error.message : "Sign-out failed",
      );
      setSigningOut(false);
    }
  }
  const expectedRepository = url.searchParams.get("repository_id");
  const renamed = repositoryCatalog.data?.redirect;
  useEffect(() => {
    if (
      !renamed ||
      !route ||
      (expectedRepository !== null && expectedRepository !== renamed.id)
    )
      return;
    const remainder = url.pathname.split("/").slice(3).join("/");
    const path = repoHref(renamed) + (remainder ? `/${remainder}` : "");
    window.location.replace(path + url.search + url.hash);
  }, [renamed, route, expectedRepository, url.pathname, url.search, url.hash]);
  const repo = repositoryCatalog.data?.repositories.find(
    (repo) =>
      repo.owner === route?.owner &&
      repo.name === route.name &&
      (expectedRepository === null || expectedRepository === repo.id),
  );
  useEffect(() => {
    if (authenticated === false) {
      document.title = "Sign in to Crab";
      return;
    }
    document.title = repo
      ? `${repo.owner}/${repo.name} · Crab`
      : subscriptions
        ? "Subscriptions · Crab"
        : watched
          ? "Watched repositories · Crab"
          : inbox
            ? "Notifications · Crab"
            : transferId
              ? "Repository transfer · Crab"
              : projectWorkspace
                ? "Projects · Crab"
                : codeSearch
                  ? "Search code · Crab"
                  : "Repositories · Crab";
  }, [
    authenticated,
    repo,
    watched,
    inbox,
    subscriptions,
    projectWorkspace,
    transferId,
    codeSearch,
  ]);
  useEffect(() => {
    if (!importedRepository || url.pathname !== "/") return;
    if (
      repositoryCatalog.data?.repositories.some(
        ({ owner, name }) =>
          owner === importedRepository.owner &&
          name === importedRepository.name,
      )
    ) {
      navigate(repoHref(importedRepository));
      setImportedRepository(undefined);
    }
  }, [importedRepository, repositoryCatalog.data, url.pathname]);
  if (session.error || session.loading || !session.data?.authenticated) {
    return (
      <TooltipProvider>
        <main id="main" className="sign-in-page" tabIndex={-1}>
          {session.error ? (
            <div className="notice error" role="alert">
              <h1>Unable to check your session</h1>
              <p>{session.error}</p>
              <Button variant="outline" onClick={session.retry}>
                Try again
              </Button>
            </div>
          ) : session.loading || !session.data ? (
            <div className="notice" role="status">
              <Spinner /> Checking your session…
            </div>
          ) : (
            <SignIn
              mode={session.data.mode}
              canSignUp={session.data.can_sign_up}
              url={url}
            />
          )}
          <div className="sign-in-appearance">
            <AppearanceControl theme={theme} setTheme={setTheme} />
          </div>
        </main>
      </TooltipProvider>
    );
  }

  return (
    <TooltipProvider>
      <ProjectCommandsProvider
        csrf={session.data?.csrf ?? ""}
        account={teamAccount}
      >
        <div className="app-shell">
          <a className="skip-link" href="#main">
            Skip to content
          </a>
          <header className="global-header">
            <Link className="brand" href="/" aria-label="Crab repositories">
              <img className="brand-mark" src={crabMark} alt="" />
            </Link>
            {repo ? (
              <div className="global-repository">
                <RepoIcon size={16} />
                <h1>
                  <Link href={repoHref(repo)}>
                    {repo.owner} <span aria-hidden="true">/</span>{" "}
                    <strong>{repo.name}</strong>
                  </Link>
                </h1>
                {repo.archived && <Badge variant="outline">Archived</Badge>}
              </div>
            ) : (
              <Link
                className="global-catalog-link"
                href={
                  projectWorkspace
                    ? `/-/organizations/${projectWorkspace}/projects`
                    : "/"
                }
              >
                {projectWorkspace ? "Projects" : "Repositories"}
              </Link>
            )}
            <div className="header-actions">
              {session.data?.authenticated && (
                <Button asChild variant="ghost" size="sm">
                  <Link
                    href="/-/code-search"
                    aria-label="Search code across repositories"
                    aria-current={codeSearch ? "page" : undefined}
                  >
                    <SearchIcon data-icon="inline-start" />
                    <span className="hidden sm:inline">Search code</span>
                  </Link>
                </Button>
              )}
              {session.data?.user && (
                <GitAccess
                  session={session.data}
                  repositories={catalog.data?.repositories ?? []}
                  onOpenChange={setGitAccessOpen}
                />
              )}
              <ThemeControl
                theme={theme}
                setTheme={setTheme}
                codeTheme={codeTheme}
                setCodeTheme={selectCodeTheme}
              />
              {session.data?.user && (
                <NotificationLink current={notifications} />
              )}
              {session.data?.authenticated && (
                <DropdownMenu modal={false}>
                  <DropdownMenuTrigger asChild>
                    <Button
                      variant="ghost"
                      size="icon-lg"
                      className="rounded-full"
                      aria-label={
                        signingOut ? "Signing out…" : "Open user menu"
                      }
                      title={session.data.user?.name ?? "User menu"}
                      disabled={signingOut}
                    >
                      {signingOut ? (
                        <Spinner />
                      ) : (
                        <Avatar aria-hidden="true">
                          <AvatarFallback>
                            {userInitial || <UserRoundIcon />}
                          </AvatarFallback>
                        </Avatar>
                      )}
                    </Button>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent
                    align="end"
                    collisionPadding={16}
                    className="w-56 max-w-[calc(100vw-2rem)]"
                  >
                    <DropdownMenuGroup>
                      <DropdownMenuLabel>
                        {session.data.user ? (
                          <>
                            <span className="block font-normal">
                              Signed in as
                            </span>
                            <span className="block truncate">
                              {session.data.user.name}
                            </span>
                          </>
                        ) : (
                          "Local session"
                        )}
                      </DropdownMenuLabel>
                      <DropdownMenuItem asChild>
                        <Link href="/-/profile">
                          <UserRoundIcon aria-hidden="true" />
                          Your profile
                        </Link>
                      </DropdownMenuItem>
                    </DropdownMenuGroup>
                    {session.data.user && (
                      <>
                        <DropdownMenuSeparator />
                        <DropdownMenuGroup>
                          <DropdownMenuItem
                            disabled={signingOut}
                            onSelect={() => void signOut()}
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
          <main id="main" tabIndex={-1}>
            {(teamCommands.intent ||
              teamCommands.failure ||
              teamCommands.resolved) && (
              <div className="mx-auto w-full max-w-6xl px-4 pt-5 sm:px-8">
                <TeamFeedback commands={teamCommands} />
              </div>
            )}
            {sessionError && (
              <p className="notice error" role="alert">
                {sessionError}
              </p>
            )}
            {codeSearch ? (
              <Suspense
                fallback={
                  <p role="status" className="notice">
                    <Spinner /> Loading code search…
                  </p>
                }
              >
                <WorkspaceCodeSearch key={teamAccount} url={url} />
              </Suspense>
            ) : subscriptions ? (
              <DiscussionSubscriptions
                key={teamAccount}
                csrf={session.data.csrf ?? ""}
                repositories={catalog.data?.repositories ?? []}
                url={url}
              />
            ) : watched ? (
              <WatchedRepositories
                key={teamAccount}
                csrf={session.data.csrf ?? ""}
              />
            ) : inbox ? (
              <Notifications
                csrf={session.data.csrf ?? ""}
                repositories={catalog.data?.repositories ?? []}
                url={url}
              />
            ) : transferId ? (
              <TransferPage
                key={transferId}
                id={transferId}
                csrf={session.data.csrf ?? ""}
              />
            ) : organizationRoute ? (
              organizationRoute[2]?.startsWith("/projects") ? (
                <OrganizationProjects
                  organization={organizationRoute[1]}
                  project={organizationRoute[3]}
                  url={url}
                />
              ) : organizationRoute[2] ? (
                <OrganizationInvitation
                  key={organizationRoute[1]}
                  id={organizationRoute[1]}
                  csrf={session.data.csrf ?? ""}
                />
              ) : (
                <OrganizationPage
                  commands={teamCommands}
                  key={organizationRoute[1]}
                  id={organizationRoute[1]}
                  url={url}
                  csrf={session.data.csrf ?? ""}
                />
              )
            ) : profileSettings ? (
              <ProfileSettings
                csrf={session.data.csrf ?? ""}
                suggestedName={session.data.user?.name ?? ""}
                section={
                  savedReplies
                    ? "saved-replies"
                    : reviewAvailability
                      ? "review-availability"
                      : "profile"
                }
              />
            ) : actorId ? (
              <UserProfile id={actorId} />
            ) : url.pathname !== "/" && !route ? (
              <div className="notice error" role="alert">
                <h1>Page not found</h1>
                <Link href="/">Back to repositories</Link>
              </div>
            ) : url.pathname === "/" ? (
              <div className="catalog">
                <div className="section-heading">
                  <h1>Your repositories</h1>
                  <div className="flex flex-wrap items-start gap-2">
                    <CreateRepository
                      csrf={session.data?.csrf ?? ""}
                      onCreated={(repository) => navigate(repoHref(repository))}
                    />
                    <GitImport
                      key={importAccount}
                      account={importAccount}
                      csrf={session.data?.csrf ?? ""}
                      defaultOwner={gitOwnerSuggestion(
                        session.data?.user?.name,
                      )}
                      onImported={(repository) =>
                        navigate(repoHref(repository))
                      }
                    />
                  </div>
                </div>
                <BrowseRepositories
                  url={url}
                  subject={session.data?.user?.subject}
                />
              </div>
            ) : (
              <Result
                state={repositoryCatalog}
                timingLabel="Repository catalog"
              >
                {() =>
                  repo ? (
                    <DiscussionRepository.Provider value={repo}>
                      <DiscussionCsrf.Provider value={session.data?.csrf ?? ""}>
                        <DiscussionCodeTheme.Provider
                          value={{
                            theme: resolved,
                            codeThemes: codeThemeChoices[codeTheme].themes,
                          }}
                        >
                          <RepositoryPage
                            key={repo.id}
                            repo={repo}
                            url={url}
                            theme={resolved}
                            codeThemes={codeThemeChoices[codeTheme].themes}
                            csrf={session.data?.csrf ?? ""}
                            identity={session.data?.user ?? null}
                            onRepositoryChanged={() => {
                              catalog.retry();
                              repositoryCatalog.retry();
                            }}
                          />
                        </DiscussionCodeTheme.Provider>
                      </DiscussionCsrf.Provider>
                    </DiscussionRepository.Provider>
                  ) : (
                    <div className="notice">
                      <h1>Repository not found</h1>
                      <Link href="/">Back to repositories</Link>
                    </div>
                  )
                }
              </Result>
            )}
          </main>
          <footer className="site-footer">
            <Link
              className="footer-brand"
              href="/"
              aria-label="Crab repositories"
            >
              <img className="footer-brand-mark" src={crabMark} alt="" />
              <strong>Crab</strong>
            </Link>
            <span className="footer-divider" aria-hidden="true">
              ·
            </span>
            <span className="footer-tagline">
              Git for any file at any scale
            </span>
          </footer>
        </div>
      </ProjectCommandsProvider>
    </TooltipProvider>
  );
}
