import { Suspense, lazy, useEffect, useLayoutEffect, useState } from "react";
import {
  ActionList,
  ActionMenu,
  BaseStyles,
  Button,
  Label,
  Spinner,
} from "@primer/react";
import { ThemeProvider } from "@primer/react/next";
import {
  AlertIcon,
  ArchiveIcon,
  CodeIcon,
  DeviceDesktopIcon,
  GitBranchIcon,
  GitPullRequestIcon,
  GearIcon,
  HistoryIcon,
  IssueOpenedIcon,
  MoonIcon,
  RepoIcon,
  SidebarCollapseIcon,
  SunIcon,
  TagIcon,
} from "@primer/octicons-react";
import {
  displayHex,
  endpoint,
  navigate,
  parentHex,
  repoHref,
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
import { FileBreadcrumb, FileNavigation } from "./file-navigation";
import {
  CreateFile,
  DeleteFile,
  EditFile,
  UploadFiles,
} from "./content-editor";
import { IssuesWorkspace } from "./issues-navigation";
import {
  RepositoryRefControls,
  RepositoryToolbar,
  revisionLabel,
} from "./repository-toolbar";
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
  import("./issues").then((module) => ({ default: module.Issues })),
);
const PullRequests = lazy(() =>
  import("./pulls").then((module) => ({ default: module.PullRequests })),
);
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
type Theme = "light" | "dark" | "auto";

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
  const resolved = theme === "auto" ? (systemDark ? "dark" : "light") : theme;
  useLayoutEffect(() => {
    const root = document.documentElement;
    root.style.colorScheme = resolved;
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
  }, [resolved]);
  const session = useRequest<Session>("/api/session");
  const [signingOut, setSigningOut] = useState(false);
  const [sessionError, setSessionError] = useState<string>();
  useEffect(() => {
    const expired = () => session.retry();
    window.addEventListener("crab-session-expired", expired);
    return () => window.removeEventListener("crab-session-expired", expired);
  });
  const catalog = useRequest<{ repositories: Repository[] }>(
    session.data?.authenticated ? "/api/repos" : null,
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
  const url = new URL(location, window.location.origin);
  const repo = catalog.data?.repositories.find(
    (repo) => `/${repo.owner}/${repo.name}` === url.pathname,
  );
  useEffect(() => {
    document.title = repo
      ? `${repo.owner}/${repo.name} · Crab`
      : "Repositories · Crab";
  }, [repo]);
  return (
    <ThemeProvider colorMode={resolved} dayScheme="light" nightScheme="dark">
      <BaseStyles className="app-shell">
        <a className="skip-link" href="#main">
          Skip to content
        </a>
        <header className="global-header">
          <Link className="brand" href="/" aria-label="Crab repositories">
            <span className="brand-mark" aria-hidden="true" />
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
              {repo.archived && <Label>Archived</Label>}
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
              />
              <span title={session.data.user.subject}>
                {session.data.user.name}
              </span>
              <Button size="small" disabled={signingOut} onClick={signOut}>
                {signingOut ? "Signing out…" : "Sign out"}
              </Button>
            </div>
          )}
          <ThemeControl theme={theme} setTheme={setTheme} resolved={resolved} />
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
              <Button onClick={session.retry}>Try again</Button>
            </div>
          ) : session.loading || !session.data ? (
            <div className="notice" role="status">
              <Spinner size="small" /> Checking your session…
            </div>
          ) : !session.data.authenticated ? (
            <div className="notice sign-in">
              <h1>Sign in to Crab</h1>
              <p>
                Use your team's identity provider to access your repositories.
              </p>
              {url.searchParams.has("auth_error") && (
                <p className="error" role="alert">
                  Sign-in could not be completed. Start again, or contact your
                  administrator if this continues.
                </p>
              )}
              <Button
                variant="primary"
                onClick={() =>
                  window.location.assign(
                    `/auth/login?${new URLSearchParams({ return_to: url.pathname + (url.searchParams.has("auth_error") ? "" : url.search) })}`,
                  )
                }
              >
                Continue to sign in
              </Button>
            </div>
          ) : (
            <Result state={catalog} showTiming={false}>
              {(data) =>
                url.pathname === "/" ? (
                  <div className="catalog">
                    <div className="section-heading">
                      <h1>Your repositories</h1>
                      <Label>{data.repositories.length}</Label>
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
                          <RepoIcon size={20} />
                          <h2>
                            <Link href={repoHref(repo)}>
                              {repo.owner} / <strong>{repo.name}</strong>
                            </Link>
                          </h2>
                          {repo.archived && <Label>Archived</Label>}
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
                    csrf={session.data?.csrf ?? ""}
                    onRepositoryChanged={catalog.retry}
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
          <span className="brand-small">Crab</span>
          <span>Self-hosted Git repositories</span>
        </footer>
      </BaseStyles>
    </ThemeProvider>
  );
}

function ThemeControl({
  theme,
  setTheme,
  resolved,
}: {
  theme: Theme;
  setTheme: (theme: Theme) => void;
  resolved: "light" | "dark";
}) {
  const choices = [
    { value: "auto", label: "System", icon: DeviceDesktopIcon },
    { value: "light", label: "Light", icon: SunIcon },
    { value: "dark", label: "Dark", icon: MoonIcon },
  ] as const;
  return (
    <div className="theme-control">
      <ActionMenu>
        <ActionMenu.Button
          aria-label="Appearance"
          leadingVisual={resolved === "dark" ? MoonIcon : SunIcon}
          size="small"
        >
          {choices.find((choice) => choice.value === theme)?.label}
        </ActionMenu.Button>
        <ActionMenu.Overlay align="end" width="small">
          <ActionList selectionVariant="single">
            {choices.map((choice) => (
              <ActionList.Item
                key={choice.value}
                selected={theme === choice.value}
                onSelect={() => setTheme(choice.value)}
              >
                <ActionList.LeadingVisual>
                  <choice.icon />
                </ActionList.LeadingVisual>
                {choice.label}
              </ActionList.Item>
            ))}
          </ActionList>
        </ActionMenu.Overlay>
      </ActionMenu>
    </div>
  );
}

function RepositoryPage({
  repo,
  url,
  theme,
  csrf,
  onRepositoryChanged,
}: {
  repo: Repository;
  url: URL;
  theme: "light" | "dark";
  csrf: string;
  onRepositoryChanged: () => void;
}) {
  const view = url.searchParams.get("view") ?? "code";
  const canWrite = repo.access === "write" && !repo.archived;
  const refs = useRequest<Refs>(
    view === "issues" || view === "labels" ? null : endpoint(repo, "refs"),
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
    url.searchParams.get("rev") ??
    visibleRefs?.head?.name ??
    visibleRefs?.refs[0]?.name;
  const selected = visibleRefs?.refs.find((ref) => ref.name === revName);
  const rev = selected?.peeled ?? selected?.oid ?? revName;
  const path = url.searchParams.get("path") ?? "";
  const kind = url.searchParams.get("kind") ?? "Tree";
  const [showTree, setShowTree] = useState(Boolean(path));
  const [searchFocusRequest, setSearchFocusRequest] = useState(0);
  useEffect(() => setShowTree(Boolean(path)), [path]);
  const overview = view === "code" && !path && !showTree;
  const fileWorkspace = view === "code" && !overview;
  const issuesView = view === "issues" || view === "labels";
  const issueNavigation =
    view === "labels" || (view === "issues" && !url.searchParams.has("issue"));
  const branch = selected?.name.startsWith("refs/heads/")
    ? selected
    : undefined;
  const canChangeFile = canWrite && branch && path && kind === "Blob";
  function selectEntry(entry: Entry) {
    navigate(
      repoHref(repo, {
        rev: branch?.name ?? rev,
        path: entry.path_hex,
        kind: entry.kind,
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
        view !== "code" ||
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
          <Link
            className={
              [
                "code",
                "create",
                "upload",
                "edit",
                "delete",
                "branches",
                "tags",
                "releases",
              ].includes(view)
                ? "active"
                : ""
            }
            aria-current={
              [
                "code",
                "create",
                "upload",
                "edit",
                "delete",
                "branches",
                "tags",
                "releases",
              ].includes(view)
                ? "page"
                : undefined
            }
            href={repoHref(repo, { rev })}
          >
            <CodeIcon /> Code
          </Link>
          <Link
            className={view === "commits" || view === "commit" ? "active" : ""}
            aria-current={
              view === "commits" || view === "commit" ? "page" : undefined
            }
            href={repoHref(repo, { rev, view: "commits" })}
          >
            <HistoryIcon /> Commits
          </Link>
          <Link
            className={view === "pulls" ? "active" : ""}
            aria-current={view === "pulls" ? "page" : undefined}
            href={repoHref(repo, { view: "pulls" })}
          >
            <GitPullRequestIcon /> Pull requests
          </Link>
          <Link
            className={view === "issues" || view === "labels" ? "active" : ""}
            aria-current={
              view === "issues" || view === "labels" ? "page" : undefined
            }
            href={repoHref(repo, { view: "issues" })}
          >
            <IssueOpenedIcon /> Issues
          </Link>
          {repo.can_admin && (
            <Link
              className={view === "settings" ? "active" : ""}
              aria-current={view === "settings" ? "page" : undefined}
              href={repoHref(repo, { view: "settings" })}
            >
              <GearIcon /> Settings
            </Link>
          )}
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
        {view === "settings" ? (
          repo.can_admin ? (
            <Result state={visibleRefState} showTiming={false}>
              {(data) => (
                <Suspense
                  fallback={
                    <div className="notice" role="status">
                      <Spinner size="small" /> Loading settings…
                    </div>
                  }
                >
                  <Settings
                    repo={repo}
                    refs={data}
                    csrf={csrf}
                    section={
                      url.searchParams.get("section") === "branches"
                        ? "branches"
                        : "general"
                    }
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
            {view === "labels" ? (
              <Suspense
                fallback={
                  <div className="notice" role="status">
                    <Spinner size="small" /> Loading labels…
                  </div>
                }
              >
                <LabelsPage repo={repo} csrf={csrf} />
              </Suspense>
            ) : (
              <Suspense
                fallback={
                  <div className="notice" role="status">
                    <Spinner size="small" /> Loading issues…
                  </div>
                }
              >
                <Issues repo={repo} url={url} csrf={csrf} />
              </Suspense>
            )}
          </IssuesWorkspace>
        ) : view === "pulls" ? (
          <Result state={visibleRefState} showTiming={false}>
            {(data) => (
              <Suspense
                fallback={
                  <div className="notice" role="status">
                    <Spinner size="small" /> Loading pull requests…
                  </div>
                }
              >
                <PullRequests
                  repo={repo}
                  refs={data}
                  url={url}
                  csrf={csrf}
                  theme={theme}
                />
              </Suspense>
            )}
          </Result>
        ) : (
          <Result state={visibleRefState} showTiming={false}>
            {(data) =>
              view === "releases" ? (
                <Suspense
                  fallback={
                    <div className="notice" role="status">
                      <Spinner size="small" /> Loading releases…
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
                      <Spinner size="small" /> Loading repository refs…
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
                        <Spinner size="small" /> Loading viewer…
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
                          revision={revisionLabel(data, revName ?? rev)}
                          archiveRevision={rev}
                          view={view}
                          path={path || undefined}
                          kind={path ? kind : undefined}
                          onRefresh={refs.retry}
                          onCreateBranch={canWrite ? createBranch : undefined}
                        />
                        <History
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
                          revision={revisionLabel(data, revName ?? rev)}
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
                        />
                      </>
                    ) : (
                      <div
                        className={
                          overview
                            ? "code-layout overview-layout"
                            : showTree
                              ? "code-layout"
                              : "code-layout no-sidebar"
                        }
                      >
                        {showTree && (
                          <aside className="tree-sidebar">
                            <div className="tree-sidebar-header">
                              <Button
                                size="small"
                                onClick={() => setShowTree(false)}
                                aria-label="Close file tree"
                                aria-expanded={true}
                              >
                                <SidebarCollapseIcon />
                              </Button>
                              <strong>Files</strong>
                            </div>
                            <div className="tree-sidebar-controls">
                              <RepositoryRefControls
                                repo={repo}
                                refs={data}
                                revision={revisionLabel(data, revName ?? rev)}
                                onSelect={(name) =>
                                  navigate(repoHref(repo, { rev: name, view }))
                                }
                                compact
                                onCreateBranch={
                                  canWrite ? createBranch : undefined
                                }
                                onCreateFile={
                                  canWrite && branch
                                    ? () =>
                                        navigate(
                                          repoHref(repo, {
                                            rev: branch.name,
                                            view: "create",
                                            path:
                                              kind === "Tree"
                                                ? path
                                                : parentHex(path),
                                            kind: "Tree",
                                          }),
                                        )
                                    : undefined
                                }
                                onUploadFiles={
                                  canWrite && branch
                                    ? () =>
                                        navigate(
                                          repoHref(repo, {
                                            rev: branch.name,
                                            view: "upload",
                                            path:
                                              kind === "Tree"
                                                ? path
                                                : parentHex(path),
                                            kind: "Tree",
                                          }),
                                        )
                                    : undefined
                                }
                                onSearch={() =>
                                  document
                                    .getElementById("repository-tree-search")
                                    ?.focus()
                                }
                              />
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
                        <div className="code-main">
                          {!showTree &&
                            (overview ? (
                              <RepositoryToolbar
                                repo={repo}
                                refs={data}
                                revision={revisionLabel(data, revName ?? rev)}
                                archiveRevision={rev}
                                view={view}
                                onRefresh={refs.retry}
                                onCreateBranch={
                                  canWrite ? createBranch : undefined
                                }
                                onBrowse={() => setShowTree(true)}
                              />
                            ) : (
                              <FileNavigation
                                repo={repo}
                                refs={data}
                                revision={revisionLabel(data, revName ?? rev)}
                                archiveRevision={rev}
                                rev={branch?.name ?? rev}
                                path={path}
                                view={view}
                                onOpenTree={() => setShowTree(true)}
                                onSearch={focusFileSearch}
                              />
                            ))}
                          {!overview && showTree && (
                            <FileBreadcrumb
                              repo={repo}
                              rev={branch?.name ?? rev}
                              path={path}
                            />
                          )}
                          {kind !== "Tree" && (
                            <LatestCommit
                              repo={repo}
                              rev={rev}
                              path={path}
                              kind={kind}
                            />
                          )}
                          <Suspense
                            fallback={
                              <div className="notice" role="status">
                                <Spinner size="small" /> Loading repository
                                content…
                              </div>
                            }
                          >
                            {kind === "Tree" ? (
                              <Directory
                                key={`${rev}:${path}`}
                                repo={repo}
                                rev={rev}
                                path={path}
                                onEntry={selectEntry}
                                header={
                                  <LatestCommit
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
                                path={path}
                                name={displayHex(path)}
                                theme={theme}
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
                                href={repoHref(repo, { view: "commits", rev })}
                              >
                                <HistoryIcon /> Activity
                              </Link>
                              <Link href={repoHref(repo, { view: "issues" })}>
                                <IssueOpenedIcon /> Issues
                              </Link>
                              <Link href={repoHref(repo, { view: "releases" })}>
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
                                        href={repoHref(repo, { rev: ref.name })}
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
  path,
  kind,
}: {
  repo: Repository;
  rev: string;
  path?: string;
  kind?: string;
}) {
  const historyHref = repoHref(repo, {
    view: "commits",
    rev,
    path: path || undefined,
    kind: path ? kind : undefined,
  });
  const state = useRequest<Commit>(
    endpoint(repo, "commit", { rev, path_hex: path || undefined }),
  );
  if (state.loading || (!state.data && !state.error))
    return (
      <div className="latest-commit" role="status">
        <Spinner size="small" />
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
          size="small"
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
    <Result state={state} showTiming={false}>
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
