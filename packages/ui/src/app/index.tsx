import { ProjectCommandsProvider } from "@/features/projects/commands";
import { OrganizationProjects } from "@/features/projects/workspace";
import { AppearanceControl, type Theme } from "@/app/theme/control";
import { GlobalHeader } from "@/app/global-header";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { SignIn } from "@/features/auth/sign-in";
import { HomeDashboard } from "@/features/home";
import { NewRepository } from "@/features/repository/create";
import { RepositoryOwnerActions } from "@/features/repository/owner-actions";
import { PersonalWork } from "@/features/personal-work";
import { Notifications } from "@/features/notifications";
import { DiscussionSubscriptions } from "@/features/notifications/subscriptions";
import { WatchedRepositories } from "@/features/notifications/watched";
import {
  OrganizationPage,
  OrganizationInvitation,
} from "@/features/organizations";
import { OrganizationDirectory } from "@/features/organizations/directory";
import { NewOrganization } from "@/features/organizations/new";
import {
  matchOrganizationRoute,
  organizationHref,
} from "@/lib/organization-route";
import type { AvatarConfig, OrganizationView, Session } from "@/lib/contracts";
import {
  TeamFeedback,
  useTeamCommands,
} from "@/features/organizations/teams/commands";
import {
  ProfileSettings,
  UserProfile,
  type SettingsSection,
} from "@/features/profile";
import { RepositoryPage } from "@/features/repository/page";
import { TransferPage } from "@/features/settings/transfer";
import { BrowseRepositories } from "@/features/repository/catalog-browse";
import {
  DiscussionCodeTheme,
  DiscussionCsrf,
  DiscussionRepository,
} from "@/components/discussion/repository-context";
import { navigate, useLocation, useRequest, type Repository } from "@/lib/api";
import {
  codeThemeChoices,
  codeThemeFrom,
  codeThemeNamesFor,
  type CodeTheme,
} from "@/lib/code-theme";
import { repoHref, repositoryRoute } from "@/lib/repository-route";
import { lazy, Suspense, useEffect, useLayoutEffect, useState } from "react";
import crabMark from "../../../web/crab.optimized.svg";

const WorkspaceCodeSearch = lazy(
  () => import("@/features/repository/workspace-code-search"),
);
const WorkSearch = lazy(() => import("@/features/personal-work/search"));

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
  const myWork = url.pathname === "/-/work";
  const workSearch = url.pathname === "/-/discussion-search";
  const inbox = url.pathname === "/-/notifications";
  const watched = url.pathname === "/-/notifications/watched";
  const subscriptions = url.pathname === "/-/notifications/subscriptions";
  const notifications = inbox || watched || subscriptions;
  const settingsSection: SettingsSection | undefined =
    url.pathname === "/-/profile" ||
    url.pathname === "/-/settings" ||
    url.pathname === "/-/settings/profile"
      ? "profile"
      : url.pathname === "/-/settings/account"
        ? "account"
        : url.pathname === "/-/settings/appearance"
          ? "appearance"
          : url.pathname === "/-/settings/notifications"
            ? "notifications"
            : url.pathname === "/-/settings/git-access"
              ? "git-access"
              : url.pathname === "/-/settings/organizations"
                ? "organizations"
                : url.pathname === "/-/settings/repositories"
                  ? "repositories"
                  : url.pathname === "/-/settings/saved-replies"
                    ? "saved-replies"
                    : url.pathname === "/-/settings/review-availability"
                      ? "review-availability"
                      : undefined;
  const profileSettings = settingsSection !== undefined;
  const actorRoute =
    /^\/-\/users\/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})(\/repositories)?$/i.exec(
      url.pathname,
    );
  const allRepositories = url.pathname === "/-/repositories";
  const personalRepositories = url.pathname === "/-/repositories/personal";
  const publicRepositories = url.pathname === "/-/public";
  const organizations = url.pathname === "/-/organizations";
  const newOrganization = url.pathname === "/-/organizations/new";
  const newRepository = url.pathname === "/-/new";
  const homeDashboard =
    url.pathname === "/" &&
    !url.searchParams.has("q") &&
    !url.searchParams.has("after");
  const repositoryCatalogPage =
    allRepositories ||
    personalRepositories ||
    publicRepositories ||
    (url.pathname === "/" && !homeDashboard);
  const transferId = /^\/-\/repository-transfers\/([0-9a-f-]{36})$/i.exec(
    url.pathname,
  )?.[1];
  const organizationRoute = matchOrganizationRoute(url.pathname);
  const route =
    profileSettings ||
    actorRoute ||
    allRepositories ||
    personalRepositories ||
    publicRepositories ||
    transferId ||
    organizationRoute ||
    organizations ||
    newOrganization ||
    newRepository ||
    notifications ||
    myWork ||
    workSearch ||
    codeSearch
      ? undefined
      : repositoryRoute(url);
  const projectWorkspace = organizationRoute?.[2]?.startsWith("/projects")
    ? organizationRoute[1]
    : undefined;
  const session = useRequest<Session>("/api/session");
  const authenticated = session.data?.authenticated;
  // The organization page and the global header share one read of the
  // organization, so the header can name it like GitHub's breadcrumb.
  const organizationPage =
    organizationRoute && !organizationRoute[2]
      ? organizationRoute[1]
      : undefined;
  const organizationView = useRequest<OrganizationView>(
    authenticated && organizationPage
      ? `/api/organizations/${organizationPage}`
      : null,
  );
  const viewedOrganization = organizationView.data?.organization;
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
  const [avatarState, setAvatarState] = useState<{
    account: string | null;
    avatar: AvatarConfig | null;
  }>();
  const currentAvatar =
    avatarState && avatarState.account === teamAccount
      ? avatarState.avatar
      : (session.data?.avatar ?? null);
  useEffect(() => {
    const expired = () => session.retry();
    window.addEventListener("crab-session-expired", expired);
    return () => window.removeEventListener("crab-session-expired", expired);
  });
  const catalog = useRequest<{ repositories: Repository[] }>(
    session.data?.authenticated && (inbox || subscriptions)
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
    session.data && requestedRepository
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
  const repositoryOwnership = useRequest<OrganizationView>(
    authenticated && repo?.organization_id
      ? `/api/organizations/${encodeURIComponent(repo.organization_id)}`
      : null,
  ).data;
  const repositoryOrganization = repositoryOwnership?.organization;
  const publicView =
    publicRepositories ||
    repo?.visibility === "public" ||
    Boolean(route && (repositoryCatalog.loading || renamed));
  useEffect(() => {
    if (authenticated === false && !publicView) {
      document.title = "Sign in to Crab";
      return;
    }
    document.title = repo
      ? `${repositoryOrganization?.name ?? repo.owner}/${repo.name} · Crab`
      : viewedOrganization
        ? `${viewedOrganization.name} · Crab`
        : newOrganization
          ? "New organization · Crab"
          : organizations
            ? "Your organizations · Crab"
            : homeDashboard
              ? "Dashboard · Crab"
              : newRepository
                ? "New repository · Crab"
                : profileSettings
                  ? "Settings · Crab"
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
                              : myWork
                                ? "My work · Crab"
                                : workSearch
                                  ? "Search issues and pull requests · Crab"
                                  : "Repositories · Crab";
  }, [
    authenticated,
    publicView,
    repo,
    homeDashboard,
    newRepository,
    profileSettings,
    repositoryOrganization,
    watched,
    inbox,
    subscriptions,
    projectWorkspace,
    transferId,
    codeSearch,
    myWork,
    workSearch,
    viewedOrganization,
    newOrganization,
    organizations,
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
  if (
    session.error ||
    session.loading ||
    !session.data ||
    (!authenticated && !publicView)
  ) {
    return (
      <TooltipProvider>
        <main id="main" className="sign-in-page" tabIndex={-1}>
          <div className="sign-in-main">
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
          </div>
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
          <GlobalHeader
            session={session.data}
            avatar={currentAvatar}
            url={url}
            repo={repo}
            repositoryOrganization={repositoryOrganization}
            contextLabel={
              viewedOrganization
                ? viewedOrganization.name
                : organizations || newOrganization
                  ? "Organizations"
                  : homeDashboard
                    ? "Dashboard"
                    : profileSettings
                      ? "Settings"
                      : projectWorkspace
                        ? "Projects"
                        : allRepositories ||
                            personalRepositories ||
                            publicRepositories
                          ? "Repositories"
                          : "Dashboard"
            }
            contextHref={
              viewedOrganization
                ? organizationHref(viewedOrganization)
                : organizations || newOrganization
                  ? "/-/organizations"
                  : homeDashboard
                    ? "/"
                    : profileSettings
                      ? "/-/settings"
                      : projectWorkspace
                        ? `/-/organizations/${projectWorkspace}/projects`
                        : allRepositories ||
                            personalRepositories ||
                            publicRepositories
                          ? "/-/repositories"
                          : "/"
            }
            notificationsActive={notifications}
            theme={theme}
            setTheme={setTheme}
            signingOut={signingOut}
            onSignOut={() => void signOut()}
            catalogRepositories={catalog.data?.repositories ?? []}
          />
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
            {homeDashboard && authenticated ? (
              <HomeDashboard
                key={teamAccount}
                session={session.data}
                avatar={currentAvatar}
                url={url}
              />
            ) : myWork ? (
              <PersonalWork key={teamAccount} url={url} />
            ) : workSearch ? (
              <Suspense
                fallback={
                  <p role="status" className="notice">
                    <Spinner /> Loading search…
                  </p>
                }
              >
                <WorkSearch key={teamAccount} url={url} />
              </Suspense>
            ) : codeSearch ? (
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
                key={teamAccount}
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
            ) : newOrganization ? (
              <NewOrganization
                key={teamAccount}
                csrf={session.data.csrf ?? ""}
              />
            ) : organizations ? (
              <div className="px-4 py-6 sm:px-8">
                <OrganizationDirectory key={teamAccount} />
              </div>
            ) : newRepository ? (
              <NewRepository
                key={teamAccount}
                session={session.data}
                avatar={currentAvatar}
                url={url}
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
                  own={organizationView}
                  url={url}
                  csrf={session.data.csrf ?? ""}
                />
              )
            ) : profileSettings ? (
              <ProfileSettings
                session={session.data}
                avatar={currentAvatar}
                onAvatarSaved={(next) =>
                  setAvatarState({ account: teamAccount, avatar: next })
                }
                csrf={session.data.csrf ?? ""}
                suggestedName={session.data.user?.name ?? ""}
                section={settingsSection}
                url={url}
                theme={theme}
                setTheme={setTheme}
                codeTheme={codeTheme}
                setCodeTheme={selectCodeTheme}
                signingOut={signingOut}
                onSignOut={() => void signOut()}
              />
            ) : actorRoute ? (
              <UserProfile
                key={actorRoute[1]}
                id={actorRoute[1]}
                repositories={Boolean(actorRoute[2])}
                url={url}
              />
            ) : !repositoryCatalogPage && !route ? (
              <div className="notice error" role="alert">
                <h1>Page not found</h1>
                <Link href="/">Back to repositories</Link>
              </div>
            ) : repositoryCatalogPage ? (
              <div className="catalog">
                <div className="section-heading">
                  <h1>
                    {publicRepositories
                      ? "Public repositories"
                      : personalRepositories
                        ? "Personal repositories"
                        : "Repositories"}
                  </h1>
                  {authenticated && !publicRepositories && (
                    <RepositoryOwnerActions
                      key={importAccount}
                      account={importAccount}
                      csrf={session.data.csrf ?? ""}
                      personal={personalRepositories}
                    />
                  )}
                </div>
                {authenticated && (
                  <nav
                    aria-label="Repository ownership"
                    className="mb-5 flex flex-wrap gap-2 border-b pb-3"
                  >
                    <Button
                      asChild
                      variant={
                        personalRepositories || publicRepositories
                          ? "ghost"
                          : "secondary"
                      }
                    >
                      <Link
                        href="/-/repositories"
                        aria-current={
                          personalRepositories || publicRepositories
                            ? undefined
                            : "page"
                        }
                      >
                        All accessible
                      </Link>
                    </Button>
                    <Button
                      asChild
                      variant={personalRepositories ? "secondary" : "ghost"}
                    >
                      <Link
                        href="/-/repositories/personal"
                        aria-current={personalRepositories ? "page" : undefined}
                      >
                        Personal
                      </Link>
                    </Button>
                    <Button
                      asChild
                      variant={publicRepositories ? "secondary" : "ghost"}
                    >
                      <Link
                        href="/-/public"
                        aria-current={publicRepositories ? "page" : undefined}
                      >
                        Public
                      </Link>
                    </Button>
                  </nav>
                )}
                <BrowseRepositories
                  url={url}
                  scope={
                    publicRepositories
                      ? { kind: "public" }
                      : personalRepositories
                        ? { kind: "personal" }
                        : { kind: "all", subject: session.data.user?.subject }
                  }
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
                            ownership={repositoryOwnership}
                            url={url}
                            theme={resolved}
                            codeThemes={codeThemeChoices[codeTheme].themes}
                            csrf={session.data?.csrf ?? ""}
                            identity={session.data?.user ?? null}
                            account={teamAccount}
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
