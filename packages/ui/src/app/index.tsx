import { ProjectCommandsProvider } from "@/features/projects/commands";
import { ThemeControl, type Theme } from "@/app/theme/control";
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
import { GitAccess } from "@/features/git-access";
import { GitImport } from "@/features/imports";
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
import {
  navigate,
  useLocation,
  useRequest,
  type Repository,
  type Session,
} from "@/lib/api";
import {
  codeThemeChoices,
  codeThemeFrom,
  codeThemeNamesFor,
  type CodeTheme,
} from "@/lib/code-theme";
import { repoHref, repositoryRoute } from "@/lib/repository-route";
import { RepoIcon } from "@primer/octicons-react";
import { useEffect, useLayoutEffect, useState } from "react";
import crabMark from "../../../web/crab.optimized.svg";

export function App() {
  const location = useLocation();
  useLayoutEffect(() => {
    if (document.activeElement === document.body)
      document.getElementById("main")?.focus();
  }, [location]);
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
  const profileSettings = url.pathname === "/-/profile";
  const actorId =
    /^\/-\/users\/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$/i.exec(
      url.pathname,
    )?.[1];
  const organizationRoute =
    /^\/-\/organizations\/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})(\/invitation)?$/i.exec(
      url.pathname,
    );
  const route =
    profileSettings || actorId || organizationRoute
      ? undefined
      : repositoryRoute(url);
  const session = useRequest<Session>("/api/session");
  const teamAccount = !session.data?.authenticated
    ? null
    : session.data.mode === "local"
      ? "local"
      : session.data.user
        ? JSON.stringify([session.data.user.issuer, session.data.user.subject])
        : null;
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
    session.data?.authenticated && (url.pathname === "/" || gitAccessOpen)
      ? "/api/repos"
      : null,
  );
  const repositoryCatalog = useRequest<{ repositories: Repository[] }>(
    session.data?.authenticated && route
      ? `/api/repos?${new URLSearchParams({ owner: route.owner, name: route.name })}`
      : null,
  );
  // A repository page must not wait for settings reads from unrelated Cells.
  // Load the full catalog only for navigation or an open Git access selector.
  const pageCatalog = route ? repositoryCatalog : catalog;
  const [importedRepository, setImportedRepository] = useState<{
    owner: string;
    name: string;
  }>();
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
  const repo = pageCatalog.data?.repositories.find(
    (repo) => repo.owner === route?.owner && repo.name === route.name,
  );
  useEffect(() => {
    document.title = repo
      ? `${repo.owner}/${repo.name} · Crab`
      : "Repositories · Crab";
  }, [repo]);
  useEffect(() => {
    if (!importedRepository || !catalog.data) return;
    if (
      catalog.data.repositories.some(
        (candidate) =>
          candidate.owner === importedRepository.owner &&
          candidate.name === importedRepository.name,
      )
    ) {
      navigate(
        `/${encodeURIComponent(importedRepository.owner)}/${encodeURIComponent(importedRepository.name)}`,
      );
      setImportedRepository(undefined);
    }
  }, [catalog.data, importedRepository]);
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
              <Link className="global-catalog-link" href="/">
                Repositories
              </Link>
            )}
            <div className="header-actions">
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
            ) : !session.data.authenticated ? (
              <div className="notice sign-in">
                <h1>Sign in to Crab</h1>
                <p>
                  {session.data.mode === "github"
                    ? "Use GitHub to access your repositories."
                    : "Use your team's identity provider to access your repositories."}
                </p>
                {url.searchParams.has("auth_error") && (
                  <p className="error" role="alert">
                    Sign-in could not be completed. Start again, or contact your
                    administrator if this continues.
                  </p>
                )}
                <Button
                  variant="default"
                  onClick={() =>
                    window.location.assign(
                      `/auth/login?${new URLSearchParams({ return_to: url.pathname + (url.searchParams.has("auth_error") ? "" : url.search) })}`,
                    )
                  }
                >
                  {session.data.mode === "github"
                    ? "Continue with GitHub"
                    : "Continue to sign in"}
                </Button>
              </div>
            ) : organizationRoute ? (
              organizationRoute[2] ? (
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
              />
            ) : actorId ? (
              <UserProfile id={actorId} />
            ) : url.pathname !== "/" && !route ? (
              <div className="notice error" role="alert">
                <h1>Page not found</h1>
                <Link href="/">Back to repositories</Link>
              </div>
            ) : (
              <Result state={pageCatalog} timingLabel="Repository catalog">
                {(data) =>
                  url.pathname === "/" ? (
                    <div className="catalog">
                      <div className="section-heading">
                        <div className="catalog-heading-title">
                          <h1>Your repositories</h1>
                          <Badge variant="outline">
                            {data.repositories.length}
                          </Badge>
                        </div>
                        <GitImport
                          csrf={session.data?.csrf ?? ""}
                          defaultOwner={gitOwnerSuggestion(
                            session.data?.user?.name,
                          )}
                          onImported={(repository) => {
                            setImportedRepository(repository);
                            catalog.retry();
                          }}
                        />
                      </div>
                      <p className="muted">Your code, in your storage.</p>
                      {data.repositories.length === 0 && (
                        <div className="notice">
                          <h2>No repositories available</h2>
                          <p>
                            Ask your administrator to grant access to your
                            account.
                          </p>
                          {session.data?.user && (
                            <p className="muted">
                              Your user ID:{" "}
                              <code>{session.data.user.subject}</code>
                            </p>
                          )}
                        </div>
                      )}
                      <div className="repo-cards">
                        {data.repositories.map((repo) => (
                          <article className="panel repo-card" key={repo.id}>
                            <div className="repo-card-heading">
                              <RepoIcon size={18} aria-hidden="true" />
                              <h2>
                                <Link href={repoHref(repo)}>
                                  {repo.owner} / <strong>{repo.name}</strong>
                                </Link>
                              </h2>
                              {repo.archived && (
                                <Badge variant="outline">Archived</Badge>
                              )}
                            </div>
                            <p className="muted">
                              {repo.description ||
                                "Browse files, history, and changes."}
                            </p>
                          </article>
                        ))}
                      </div>
                    </div>
                  ) : repo ? (
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
