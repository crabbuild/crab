import { Badge } from "@/components/ui/badge";
import { TooltipProvider } from "@/components/ui/tooltip";
import { Spinner } from "@/components/ui/spinner";
import { Button } from "@/components/ui/button";
import {
  repoHref,
  repositoryRoute,
  repositoryRef,
  revisionAddress,
} from "./repository-route";
import {
  Suspense,
  lazy,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
} from "react";
import {
  AlertIcon,
  ArchiveIcon,
  CodeIcon,
  GitBranchIcon,
  GitPullRequestIcon,
  GearIcon,
  HistoryIcon,
  IssueOpenedIcon,
  RepoIcon,
  SidebarCollapseIcon,
  TagIcon,
} from "@primer/octicons-react";
import {
  displayHex,
  endpoint,
  navigate,
  parentHex,
  useLocation,
  useRequest,
  type Entry,
  type Ref,
  type Refs,
  type Repository,
  type Commit,
  type Session,
} from "./api";
import { Link, Result, date, short } from "./ui";
import { GitAccess } from "./git-access";
import { GitImport } from "./git-import";
import { gitOwnerSuggestion } from "./git-import-source";
import { FileBreadcrumb, FileNavigation } from "./file-navigation";
import { PaneResizer } from "./pane-resizer";
import {
  codeThemeChoices,
  codeThemeFrom,
  codeThemeNamesFor,
  type CodeTheme,
  type CodeThemes,
} from "./code-theme";
import {
  CreateFile,
  DeleteFile,
  EditFile,
  UploadFiles,
} from "./content-editor";
import { IssuesWorkspace } from "./issues-navigation";
import { RepositoryRefControls, RepositoryToolbar } from "./repository-toolbar";
import crabMark from "../../web/crab.optimized.svg";
const RepositoryTree = lazy(() =>
  import("./tree").then((module) => ({ default: module.RepositoryTree })),
);
const Directory = lazy(() =>
  import("./browse").then((module) => ({ default: module.Directory })),
);
const FileView = lazy(() =>
  import("./content").then((module) => ({ default: module.FileView })),
);
const History = lazy(() =>
  import("./browse").then((module) => ({ default: module.History })),
);
const CommitView = lazy(() =>
  import("./content").then((module) => ({ default: module.CommitView })),
);
const Issues = lazy(() =>
  import("@/features/issues").then((module) => ({ default: module.Issues })),
);
const PullRequests = lazy(() =>
  import("./pulls").then((module) => ({ default: module.PullRequests })),
);
const ActivityPage = lazy(() => import("./collaboration/activity"));
const MilestonesPage = lazy(() => import("./collaboration/milestones"));
const LabelsPage = lazy(() =>
  import("./discussion-labels").then((module) => ({
    default: module.LabelsPage,
  })),
);
const RefsPage = lazy(() =>
  import("./refs").then((module) => ({ default: module.RefsPage })),
);
const Releases = lazy(() =>
  import("./releases").then((module) => ({ default: module.Releases })),
);
const Settings = lazy(() =>
  import("./settings").then((module) => ({ default: module.Settings })),
);
import { ThemeControl, type Theme } from "@/app/theme/control";
const DEFAULT_TREE_PANE_WIDTH = 356;
const MIN_TREE_PANE_WIDTH = 240;
const MAX_TREE_PANE_WIDTH = 640;
const TREE_PANE_KEYBOARD_STEP = 24;

function clampTreePaneWidth(width: number, max = MAX_TREE_PANE_WIDTH) {
  return Math.min(max, Math.max(MIN_TREE_PANE_WIDTH, Math.round(width)));
}

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
  const route = repositoryRoute(url);
  const session = useRequest<Session>("/api/session");
  const [signingOut, setSigningOut] = useState(false);
  const [sessionError, setSessionError] = useState<string>();
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
          {session.data?.user && (
            <div className="session-control">
              <GitAccess
                session={session.data}
                repositories={catalog.data?.repositories ?? []}
                onOpenChange={setGitAccessOpen}
              />
              <span title={session.data.user.subject}>
                {session.data.user.name}
              </span>
              <Button
                variant="outline"
                size="sm"
                disabled={signingOut}
                onClick={signOut}
              >
                {signingOut ? "Signing out…" : "Sign out"}
              </Button>
            </div>
          )}
          <ThemeControl
            theme={theme}
            setTheme={setTheme}
            codeTheme={codeTheme}
            setCodeTheme={selectCodeTheme}
          />
        </header>
        <main id="main" tabIndex={-1}>
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
                        <article
                          className="panel repo-card"
                          key={`${repo.owner}/${repo.name}`}
                        >
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
                    key={`${repo.owner}/${repo.name}`}
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
          <span className="footer-tagline">Git for any file at any scale</span>
        </footer>
      </div>
    </TooltipProvider>
  );
}

function RepositoryPage({
  repo,
  url,
  theme,
  codeThemes,
  csrf,
  identity,
  onRepositoryChanged,
}: {
  repo: Repository;
  url: URL;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  csrf: string;
  identity: Session["user"];
  onRepositoryChanged: () => void;
}) {
  const route = repositoryRoute(url);
  const view = route?.view ?? "code";
  const canWrite = repo.access === "write" && !repo.archived;
  const refs = useRequest<Refs>(
    view === "issues" ||
      view === "labels" ||
      view === "milestones" ||
      view === "activity"
      ? null
      : endpoint(repo, "refs"),
  );
  const [createdRefs, setCreatedRefs] = useState<Ref[]>([]);
  const [deletedRefs, setDeletedRefs] = useState<string[]>([]);
  useEffect(() => {
    if (!refs.data) return;
    setDeletedRefs((current) =>
      current.filter((name) =>
        refs.data?.refs.some((reference) => reference.name === name),
      ),
    );
  }, [refs.data]);
  const visibleRefs = refs.data
    ? {
        ...refs.data,
        refs: [
          ...createdRefs,
          ...refs.data.refs.filter(
            (ref) =>
              !deletedRefs.includes(ref.name) &&
              !createdRefs.some((created) => created.name === ref.name),
          ),
        ],
      }
    : undefined;
  const visibleRefState = { ...refs, data: visibleRefs };
  const revName =
    (route?.rev === "HEAD" ? undefined : route?.rev) ??
    visibleRefs?.head?.name ??
    visibleRefs?.refs[0]?.name;
  const resolvedRoute = repositoryRoute(url, visibleRefs);
  const requestedRevision =
    resolvedRoute?.rev === "HEAD" ? undefined : resolvedRoute?.rev;
  const revision = requestedRevision ?? revName;
  const selected = repositoryRef(visibleRefs, revision);
  const rev = selected?.peeled ?? selected?.oid ?? revision;
  const navigationRevision =
    selected && visibleRefs
      ? revisionAddress(visibleRefs, selected.name)
      : revision;
  const path = resolvedRoute?.path ?? "";
  const kind = resolvedRoute?.kind ?? "Tree";
  const [showTree, setShowTree] = useState(Boolean(path));
  const [searchFocusRequest, setSearchFocusRequest] = useState(0);
  const [treePaneWidth, setTreePaneWidth] = useState(DEFAULT_TREE_PANE_WIDTH);
  const [treePaneMax, setTreePaneMax] = useState(MAX_TREE_PANE_WIDTH);
  const fileLayout = useRef<HTMLDivElement>(null);
  useEffect(() => setShowTree(Boolean(path)), [path]);
  useLayoutEffect(() => {
    const layout = fileLayout.current;
    if (!layout || !showTree) return;
    const updateLimit = () => {
      if (matchMedia("(max-width: 640px)").matches) return;
      const max = Math.max(
        MIN_TREE_PANE_WIDTH,
        Math.min(MAX_TREE_PANE_WIDTH, Math.floor(layout.clientWidth / 2)),
      );
      setTreePaneMax(max);
      setTreePaneWidth((width) => clampTreePaneWidth(width, max));
    };
    updateLimit();
    const observer = new ResizeObserver(updateLimit);
    observer.observe(layout);
    return () => observer.disconnect();
  }, [showTree]);
  const overview = view === "code" && !path && !showTree;
  const fileWorkspace = (view === "code" || view === "blame") && !overview;
  const issuesView =
    view === "issues" || view === "labels" || view === "milestones";
  const issueNavigation =
    view === "labels" ||
    view === "milestones" ||
    (view === "issues" && !route?.issue);
  const branch = selected?.name.startsWith("refs/heads/")
    ? selected
    : undefined;
  const canChangeFile = canWrite && branch && path && kind === "Blob";
  function selectEntry(entry: Entry) {
    navigate(
      repoHref(repo, {
        rev: navigationRevision,
        path: entry.path_hex,
        kind: entry.kind,
      }),
    );
  }
  function openFileAction(view: "create" | "upload") {
    if (!branch) return;
    navigate(
      repoHref(repo, {
        rev: branch.name,
        view,
        path: kind === "Tree" ? path : parentHex(path),
      }),
    );
  }
  function focusFileSearch() {
    setShowTree(true);
    setSearchFocusRequest((request) => request + 1);
  }
  async function createBranch(name: string) {
    if (!rev)
      throw new Error("Select a source commit before creating a branch");
    const response = await fetch(endpoint(repo, "branches"), {
      method: "POST",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
        "X-CSRF-Token": csrf,
      },
      body: JSON.stringify({ name, source_oid: rev }),
    });
    if (response.status === 401)
      window.dispatchEvent(new Event("crab-session-expired"));
    const body: unknown = await response.json();
    if (!response.ok) {
      const failure = body as { error?: { code?: string; message?: string } };
      if (failure.error?.code === "branch_changed") refs.retry();
      throw new Error(
        failure.error?.message ?? `Request failed (${response.status})`,
      );
    }
    const created = body as { branch: string; commit: string };
    setDeletedRefs((current) =>
      current.filter((ref) => ref !== created.branch),
    );
    setCreatedRefs((current) => [
      { name: created.branch, oid: created.commit },
      ...current.filter((ref) => ref.name !== created.branch),
    ]);
    navigate(
      repoHref(repo, {
        rev: created.branch,
        view,
        path: path || undefined,
        kind: path ? kind : undefined,
      }),
    );
  }
  async function deleteBranch(name: string, expectedOid: string) {
    const response = await fetch(endpoint(repo, "branches"), {
      method: "DELETE",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
        "X-CSRF-Token": csrf,
      },
      body: JSON.stringify({
        name: name.replace(/^refs\/heads\//, ""),
        expected_oid: expectedOid,
      }),
    });
    if (response.status === 401)
      window.dispatchEvent(new Event("crab-session-expired"));
    const body: unknown = await response.json();
    if (!response.ok) {
      const failure = body as { error?: { message?: string } };
      throw new Error(
        failure.error?.message ?? `Request failed (${response.status})`,
      );
    }
    const deleted = body as { branch: string };
    setCreatedRefs((current) =>
      current.filter((ref) => ref.name !== deleted.branch),
    );
    setDeletedRefs((current) => [...new Set([...current, deleted.branch])]);
    refs.retry();
  }
  useEffect(() => {
    const focusSearch = (event: KeyboardEvent) => {
      const target = event.target;
      if (
        !["code", "blame"].includes(view) ||
        event.key.toLowerCase() !== "t" ||
        event.metaKey ||
        event.ctrlKey ||
        event.altKey ||
        (target instanceof HTMLElement &&
          (target.isContentEditable ||
            ["INPUT", "SELECT", "TEXTAREA"].includes(target.tagName)))
      )
        return;
      event.preventDefault();
      focusFileSearch();
    };
    window.addEventListener("keydown", focusSearch);
    return () => window.removeEventListener("keydown", focusSearch);
  });
  return (
    <>
      <div className="repo-header">
        <nav aria-label="Repository">
          {[
            {
              view: "code",
              label: "Code",
              icon: CodeIcon,
              active:
                !issuesView &&
                view !== "pulls" &&
                view !== "settings" &&
                view !== "activity",
            },
            {
              view: "issues",
              label: "Issues",
              icon: IssueOpenedIcon,
              active: issuesView,
            },
            {
              view: "pulls",
              label: "Pull requests",
              icon: GitPullRequestIcon,
              active: view === "pulls",
            },
            {
              view: "activity",
              label: "Activity",
              icon: HistoryIcon,
              active: view === "activity",
            },
            ...(repo.can_admin
              ? [
                  {
                    view: "settings",
                    label: "Settings",
                    icon: GearIcon,
                    active: view === "settings",
                  },
                ]
              : []),
          ].map(({ view: tab, label, icon: Icon, active }) => (
            <Link
              key={tab}
              className={active ? "active" : ""}
              aria-current={active ? "page" : undefined}
              href={repoHref(repo, { view: tab })}
            >
              <Icon /> {label}
            </Link>
          ))}
        </nav>
      </div>
      {repo.archived && (
        <div className="archived-banner" role="status">
          <ArchiveIcon size={16} />
          <span>This repository was archived and is read-only.</span>
        </div>
      )}
      <div
        className={`repo-body${overview ? " repo-overview" : ""}${fileWorkspace ? " repo-file-workspace" : ""}${issueNavigation ? " repo-issues-workspace" : ""}${view === "settings" ? " repo-settings-workspace" : ""}`}
      >
        {view === "activity" ? (
          <Suspense
            fallback={
              <div className="notice" role="status">
                <Spinner /> Loading activity…
              </div>
            }
          >
            <ActivityPage repo={repo} />
          </Suspense>
        ) : view === "settings" ? (
          repo.can_admin ? (
            <Result state={visibleRefState} timingLabel="Repository references">
              {(data) => (
                <Suspense
                  fallback={
                    <div className="notice" role="status">
                      <Spinner /> Loading settings…
                    </div>
                  }
                >
                  <Settings
                    repo={repo}
                    refs={data}
                    csrf={csrf}
                    section={
                      route?.section === "branches"
                        ? "branches"
                        : route?.section === "members"
                          ? "members"
                          : "general"
                    }
                    identity={identity}
                    onDefaultChanged={refs.retry}
                    onRepositoryChanged={onRepositoryChanged}
                  />
                </Suspense>
              )}
            </Result>
          ) : (
            <div className="notice error" role="alert">
              Administrator access is required to view repository settings.
            </div>
          )
        ) : issuesView ? (
          <IssuesWorkspace
            repo={repo}
            view={view}
            showNavigation={issueNavigation}
          >
            {view === "milestones" ? (
              <Suspense
                fallback={
                  <div className="notice" role="status">
                    <Spinner /> Loading milestones…
                  </div>
                }
              >
                <MilestonesPage repo={repo} url={url} csrf={csrf} />
              </Suspense>
            ) : view === "labels" ? (
              <Suspense
                fallback={
                  <div className="notice" role="status">
                    <Spinner /> Loading labels…
                  </div>
                }
              >
                <LabelsPage repo={repo} csrf={csrf} />
              </Suspense>
            ) : (
              <Suspense
                fallback={
                  <div className="notice" role="status">
                    <Spinner /> Loading issues…
                  </div>
                }
              >
                <Issues repo={repo} url={url} csrf={csrf} />
              </Suspense>
            )}
          </IssuesWorkspace>
        ) : view === "pulls" ? (
          <Result state={visibleRefState} timingLabel="Repository references">
            {(data) => (
              <Suspense
                fallback={
                  <div className="notice" role="status">
                    <Spinner /> Loading pulls…
                  </div>
                }
              >
                <PullRequests
                  repo={repo}
                  refs={data}
                  url={url}
                  csrf={csrf}
                  theme={theme}
                  codeThemes={codeThemes}
                />
              </Suspense>
            )}
          </Result>
        ) : (
          <Result state={visibleRefState} timingLabel="Repository references">
            {(data) =>
              view === "releases" ? (
                <Suspense
                  fallback={
                    <div className="notice" role="status">
                      <Spinner /> Loading releases…
                    </div>
                  }
                >
                  <Releases
                    repo={repo}
                    refs={data}
                    url={url}
                    csrf={csrf}
                    onPublished={refs.retry}
                  />
                </Suspense>
              ) : view === "branches" || view === "tags" ? (
                <Suspense
                  fallback={
                    <div className="notice" role="status">
                      <Spinner /> Loading repository refs…
                    </div>
                  }
                >
                  <RefsPage
                    key={view}
                    repo={repo}
                    refs={data}
                    type={view === "branches" ? "branches" : "tags"}
                    onDeleteBranch={canWrite ? deleteBranch : undefined}
                  />
                </Suspense>
              ) : !data.refs.length ? (
                <div className="notice">
                  <h2>This repository is empty</h2>
                  <p>Push an initial commit with Crab to start browsing.</p>
                </div>
              ) : rev ? (
                <>
                  <Suspense
                    fallback={
                      <div className="notice" role="status">
                        <Spinner /> Loading viewer…
                      </div>
                    }
                  >
                    {view === "upload" ? (
                      canWrite && branch ? (
                        <UploadFiles
                          repo={repo}
                          branch={branch.name}
                          expectedHead={branch.oid}
                          directoryHex={
                            kind === "Tree" ? path : parentHex(path)
                          }
                          csrf={csrf}
                        />
                      ) : (
                        <div className="notice error" role="alert">
                          Write access to a branch is required to upload files.
                        </div>
                      )
                    ) : view === "create" ? (
                      canWrite && branch ? (
                        <CreateFile
                          repo={repo}
                          branch={branch.name}
                          expectedHead={branch.oid}
                          directoryHex={
                            kind === "Tree" ? path : parentHex(path)
                          }
                          csrf={csrf}
                        />
                      ) : (
                        <div className="notice error" role="alert">
                          Write access to a branch is required to create files.
                        </div>
                      )
                    ) : view === "edit" && canChangeFile ? (
                      <EditFile
                        repo={repo}
                        branch={branch.name}
                        expectedHead={branch.oid}
                        pathHex={path}
                        csrf={csrf}
                      />
                    ) : view === "delete" && canChangeFile ? (
                      <DeleteFile
                        repo={repo}
                        branch={branch.name}
                        expectedHead={branch.oid}
                        pathHex={path}
                        csrf={csrf}
                      />
                    ) : view === "edit" || view === "delete" ? (
                      <div className="notice error" role="alert">
                        Write access to a branch file is required for this
                        change.
                      </div>
                    ) : view === "commits" ? (
                      <>
                        <RepositoryToolbar
                          repo={repo}
                          refs={data}
                          revision={selected?.name ?? revision ?? rev}
                          archiveRevision={rev}
                          view={view}
                          path={path || undefined}
                          kind={path ? kind : undefined}
                          onRefresh={refs.retry}
                          onCreateBranch={canWrite ? createBranch : undefined}
                        />
                        <History
                          revision={navigationRevision ?? rev}
                          key={`${rev}:${path}`}
                          repo={repo}
                          rev={rev}
                          path={path}
                          kind={kind}
                        />
                      </>
                    ) : view === "commit" ? (
                      <>
                        <RepositoryToolbar
                          repo={repo}
                          refs={data}
                          revision={selected?.name ?? revision ?? rev}
                          archiveRevision={rev}
                          view={view}
                          onRefresh={refs.retry}
                          onCreateBranch={canWrite ? createBranch : undefined}
                        />
                        <CommitView
                          key={rev}
                          repo={repo}
                          rev={rev}
                          theme={theme}
                          codeThemes={codeThemes}
                        />
                      </>
                    ) : (
                      <>
                        {overview && (
                          <RepositoryToolbar
                            repo={repo}
                            refs={data}
                            revision={selected?.name ?? revision ?? rev}
                            archiveRevision={rev}
                            view={view}
                            onRefresh={refs.retry}
                            onCreateBranch={canWrite ? createBranch : undefined}
                            onBrowse={focusFileSearch}
                            onCreateFile={
                              canWrite && branch
                                ? () => openFileAction("create")
                                : undefined
                            }
                            onUploadFiles={
                              canWrite && branch
                                ? () => openFileAction("upload")
                                : undefined
                            }
                          />
                        )}
                        <div
                          ref={fileLayout}
                          className={
                            overview
                              ? "code-layout overview-layout"
                              : showTree
                                ? "code-layout"
                                : "code-layout no-sidebar"
                          }
                          style={
                            {
                              "--tree-pane-width": `${treePaneWidth}px`,
                            } as CSSProperties
                          }
                        >
                          {showTree && (
                            <aside
                              id="repository-file-tree"
                              className="tree-sidebar"
                            >
                              <div className="tree-sidebar-controls">
                                <RepositoryRefControls
                                  repo={repo}
                                  refs={data}
                                  revision={selected?.name ?? revision ?? rev}
                                  onSelect={(name) =>
                                    navigate(
                                      repoHref(repo, {
                                        rev: revisionAddress(data, name),
                                        view,
                                        path,
                                        kind,
                                      }),
                                    )
                                  }
                                  compact
                                  onCreateBranch={
                                    canWrite ? createBranch : undefined
                                  }
                                  onCreateFile={
                                    canWrite && branch
                                      ? () => openFileAction("create")
                                      : undefined
                                  }
                                  onUploadFiles={
                                    canWrite && branch
                                      ? () => openFileAction("upload")
                                      : undefined
                                  }
                                  onSearch={() =>
                                    document
                                      .getElementById("repository-tree-search")
                                      ?.focus()
                                  }
                                />
                                <Button
                                  size="icon-sm"
                                  onClick={() => setShowTree(false)}
                                  aria-label="Close file tree"
                                  aria-expanded={true}
                                  variant="outline"
                                >
                                  <SidebarCollapseIcon data-icon="inline-start" />
                                </Button>
                              </div>
                              <RepositoryTree
                                key={rev}
                                repo={repo}
                                rev={rev}
                                activePath={path ? displayHex(path) : undefined}
                                activePathHex={path || undefined}
                                focusRequest={searchFocusRequest}
                                onSelect={selectEntry}
                              />
                            </aside>
                          )}
                          {showTree && (
                            <PaneResizer
                              className="tree-resizer"
                              label="Resize file tree pane"
                              controls="repository-file-tree repository-file-content"
                              value={treePaneWidth}
                              min={MIN_TREE_PANE_WIDTH}
                              max={treePaneMax}
                              defaultValue={DEFAULT_TREE_PANE_WIDTH}
                              step={TREE_PANE_KEYBOARD_STEP}
                              unit="pixels"
                              valueText={`${treePaneWidth} pixels wide`}
                              onChange={setTreePaneWidth}
                            />
                          )}
                          <div
                            id="repository-file-content"
                            className={`code-main${kind === "Blob" ? " code-main-file" : ""}`}
                          >
                            {!showTree && !overview && (
                              <FileNavigation
                                repo={repo}
                                refs={data}
                                revision={selected?.name ?? revision ?? rev}
                                archiveRevision={rev}
                                rev={navigationRevision ?? rev}
                                path={path}
                                kind={kind}
                                view={view}
                                onOpenTree={() => setShowTree(true)}
                                onSearch={focusFileSearch}
                              />
                            )}
                            {!overview && showTree && (
                              <FileBreadcrumb
                                repo={repo}
                                rev={navigationRevision ?? rev}
                                path={path}
                              />
                            )}
                            {kind !== "Tree" && (
                              <LatestCommit
                                revision={navigationRevision ?? rev}
                                repo={repo}
                                rev={rev}
                                path={path}
                                kind={kind}
                              />
                            )}
                            <Suspense
                              fallback={
                                <div className="notice" role="status">
                                  <Spinner /> Loading repository content…
                                </div>
                              }
                            >
                              {kind === "Tree" ? (
                                <Directory
                                  revision={navigationRevision ?? rev}
                                  key={`${rev}:${path}`}
                                  repo={repo}
                                  rev={rev}
                                  path={path}
                                  theme={theme}
                                  codeThemes={codeThemes}
                                  onEntry={selectEntry}
                                  header={
                                    <LatestCommit
                                      revision={navigationRevision ?? rev}
                                      repo={repo}
                                      rev={rev}
                                      path={path}
                                      kind="Tree"
                                    />
                                  }
                                />
                              ) : kind === "Submodule" ? (
                                <div className="notice">
                                  This entry points to a commit in a submodule.
                                </div>
                              ) : (
                                <FileView
                                  key={`${rev}:${path}`}
                                  repo={repo}
                                  rev={rev}
                                  revision={navigationRevision ?? rev}
                                  blameView={view === "blame"}
                                  path={path}
                                  name={displayHex(path)}
                                  theme={theme}
                                  codeThemes={codeThemes}
                                  write={
                                    canChangeFile
                                      ? {
                                          branch: branch.name,
                                        }
                                      : undefined
                                  }
                                />
                              )}
                            </Suspense>
                          </div>
                          {overview && (
                            <aside
                              className="repository-about"
                              aria-label="About this repository"
                            >
                              <h2>About</h2>
                              <p>
                                {repo.description || "No description provided."}
                              </p>
                              <div className="about-links">
                                <Link
                                  href={repoHref(repo, {
                                    view: "activity",
                                  })}
                                >
                                  <HistoryIcon /> Activity
                                </Link>
                                <Link href={repoHref(repo, { view: "issues" })}>
                                  <IssueOpenedIcon /> Issues
                                </Link>
                                <Link
                                  href={repoHref(repo, { view: "releases" })}
                                >
                                  <TagIcon /> Releases
                                </Link>
                              </div>
                              {data.refs.some((ref) =>
                                ref.name.startsWith("refs/heads/"),
                              ) && (
                                <div className="about-section">
                                  <h3>Branches</h3>
                                  <div className="about-links">
                                    {data.refs
                                      .filter((ref) =>
                                        ref.name.startsWith("refs/heads/"),
                                      )
                                      .slice(0, 5)
                                      .map((ref) => (
                                        <Link
                                          key={ref.name}
                                          href={repoHref(repo, {
                                            rev: ref.name,
                                          })}
                                        >
                                          <GitBranchIcon />
                                          {ref.name.slice("refs/heads/".length)}
                                        </Link>
                                      ))}
                                  </div>
                                </div>
                              )}
                            </aside>
                          )}
                        </div>
                      </>
                    )}
                  </Suspense>
                </>
              ) : null
            }
          </Result>
        )}
      </div>
    </>
  );
}

function LatestCommit({
  repo,
  rev,
  revision,
  path,
  kind,
}: {
  repo: Repository;
  rev: string;
  revision: string;
  path?: string;
  kind?: string;
}) {
  const historyHref = repoHref(repo, {
    view: "commits",
    rev: revision,
    path: path || undefined,
    kind: path ? kind : undefined,
  });
  const state = useRequest<Commit>(
    endpoint(repo, "commit", { rev, path_hex: path || undefined }),
  );
  if (state.loading || (!state.data && !state.error))
    return (
      <div className="latest-commit" role="status">
        <Spinner />
        <span className="muted">Loading latest commit…</span>
      </div>
    );
  if (state.error)
    return (
      <div className="latest-commit latest-commit-error" role="alert">
        <AlertIcon aria-hidden="true" />
        <span className="truncate">
          <strong>Latest commit unavailable.</strong> {state.error}
        </span>
        <Button
          variant="outline"
          size="sm"
          aria-label="Retry latest commit"
          onClick={state.retry}
        >
          Retry
        </Button>
        <Link className="commit-history-link" href={historyHref}>
          <HistoryIcon /> History
        </Link>
      </div>
    );
  return (
    <Result state={state}>
      {(commit) => (
        <div className="latest-commit">
          <span className="avatar" aria-hidden="true">
            {commit.author.slice(0, 1).toUpperCase()}
          </span>
          <strong>{commit.author}</strong>
          <Link
            className="truncate"
            href={repoHref(repo, { view: "commit", rev: commit.oid })}
          >
            {commit.message.split("\n")[0]}
          </Link>
          <Link
            className="oid"
            href={repoHref(repo, { view: "commit", rev: commit.oid })}
          >
            {short(commit.oid)}
          </Link>
          <span className="muted commit-date">
            {date(commit.author_seconds)}
          </span>
          <Link className="commit-history-link" href={historyHref}>
            <HistoryIcon /> History
          </Link>
        </div>
      )}
    </Result>
  );
}
