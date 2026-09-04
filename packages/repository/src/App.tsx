import { Suspense, lazy, useEffect, useState } from "react";
import { BaseStyles, Button, Label, Spinner } from "@primer/react";
import { ThemeProvider } from "@primer/react/next";
import {
  CodeIcon,
  GitBranchIcon,
  HistoryIcon,
  RepoIcon,
  SunIcon,
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
  type Refs,
  type Repository,
  type Commit,
  type Session,
} from "./api";
import { Link, Result, date, short } from "./ui";
import { CloneMenu, GitAccess } from "./git-access";
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
type Theme = "light" | "dark" | "auto";

export function App() {
  const location = useLocation();
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
            <span className="brand-mark" aria-hidden="true">
              C
            </span>
          </Link>
          <Link href="/">Repositories</Link>
          {repo && (
            <>
              <span className="muted">/</span>
              <span>{repo.owner}</span>
              <span className="muted">/</span>
              <strong>{repo.name}</strong>
            </>
          )}
          {session.data?.user && (
            <div className="session-control">
              <GitAccess session={session.data} />
              <span title={session.data.user.subject}>
                {session.data.user.name}
              </span>
              <Button size="small" disabled={signingOut} onClick={signOut}>
                {signingOut ? "Signing out…" : "Sign out"}
              </Button>
            </div>
          )}
          <div className="theme-control">
            <SunIcon />
            <label className="sr-only" htmlFor="theme">
              Appearance
            </label>
            <select
              id="theme"
              value={theme}
              onChange={(event) => setTheme(event.target.value as Theme)}
            >
              <option value="auto">System</option>
              <option value="light">Light</option>
              <option value="dark">Dark</option>
            </select>
          </div>
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
            <Result state={catalog}>
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

function RepositoryPage({
  repo,
  url,
  theme,
}: {
  repo: Repository;
  url: URL;
  theme: "light" | "dark";
}) {
  const refs = useRequest<Refs>(endpoint(repo, "refs"));
  const revName = url.searchParams.get("rev") ?? refs.data?.head?.name;
  const selected = refs.data?.refs.find((ref) => ref.name === revName);
  const rev = selected?.peeled ?? selected?.oid ?? revName;
  const path = url.searchParams.get("path") ?? "";
  const kind = url.searchParams.get("kind") ?? "Tree";
  const view = url.searchParams.get("view") ?? "code";
  const [showTree, setShowTree] = useState(true);
  function selectEntry(entry: Entry) {
    navigate(repoHref(repo, { rev, path: entry.path_hex, kind: entry.kind }));
  }
  return (
    <>
      <div className="repo-header">
        <CloneMenu repo={repo} />
        <div className="repo-title">
          <RepoIcon size={20} />
          <h1>
            <Link href={repoHref(repo)}>
              {repo.owner} / <strong>{repo.name}</strong>
            </Link>
          </h1>
          <Label>Object storage</Label>
        </div>
        <nav aria-label="Repository">
          <Link
            className={view === "code" ? "active" : ""}
            aria-current={view === "code" ? "page" : undefined}
            href={repoHref(repo, { rev })}
          >
            <CodeIcon /> Code
          </Link>
          <Link
            className={view !== "code" ? "active" : ""}
            aria-current={view !== "code" ? "page" : undefined}
            href={repoHref(repo, { rev, view: "commits" })}
          >
            <HistoryIcon /> Commits
          </Link>
        </nav>
      </div>
      <div className="repo-body">
        <Result state={refs}>
          {(data) =>
            !data.head ? (
              <div className="notice">
                <h2>This repository is empty</h2>
                <p>Push an initial commit with Crab to start browsing.</p>
              </div>
            ) : rev ? (
              <>
                <div className="toolbar">
                  <div className="row">
                    <GitBranchIcon />
                    <label className="sr-only" htmlFor="revision">
                      Branch or tag
                    </label>
                    <select
                      id="revision"
                      value={selected ? revName : rev}
                      onChange={(event) =>
                        navigate(
                          repoHref(repo, { rev: event.target.value, view }),
                        )
                      }
                    >
                      {!selected && <option value={rev}>{short(rev)}</option>}
                      {data.refs.map((ref) => (
                        <option key={ref.name} value={ref.name}>
                          {ref.name.replace(/^refs\/(heads|tags)\//, "")}
                        </option>
                      ))}
                    </select>
                    <span className="muted">
                      {
                        data.refs.filter((ref) =>
                          ref.name.startsWith("refs/heads/"),
                        ).length
                      }{" "}
                      branches
                    </span>
                    <span className="muted">
                      {
                        data.refs.filter((ref) =>
                          ref.name.startsWith("refs/tags/"),
                        ).length
                      }{" "}
                      tags
                    </span>
                  </div>
                  <Button onClick={refs.retry}>Refresh</Button>
                </div>
                <Suspense
                  fallback={
                    <div className="notice" role="status">
                      <Spinner size="small" /> Loading viewer…
                    </div>
                  }
                >
                  {view === "commits" ? (
                    <History key={rev} repo={repo} rev={rev} />
                  ) : view === "commit" ? (
                    <CommitView key={rev} repo={repo} rev={rev} theme={theme} />
                  ) : (
                    <div
                      className={
                        showTree ? "code-layout" : "code-layout no-sidebar"
                      }
                    >
                      {showTree && (
                        <aside className="tree-sidebar">
                          <div className="panel-header">
                            <strong>Files</strong>
                            <Button
                              size="small"
                              onClick={() => setShowTree(false)}
                            >
                              Hide
                            </Button>
                          </div>
                          <RepositoryTree
                            key={rev}
                            repo={repo}
                            rev={rev}
                            onSelect={selectEntry}
                          />
                        </aside>
                      )}
                      <div className="code-main">
                        <div className="breadcrumb">
                          <Button
                            size="small"
                            onClick={() => setShowTree((value) => !value)}
                            aria-expanded={showTree}
                          >
                            Files
                          </Button>
                          <Link href={repoHref(repo, { rev })}>
                            {repo.name}
                          </Link>
                          {path && (
                            <>
                              <span>/</span>
                              <Link
                                href={repoHref(repo, {
                                  rev,
                                  path: parentHex(path),
                                })}
                              >
                                …
                              </Link>
                              <span>/</span>
                              <strong>
                                {displayHex(path).split("/").pop()}
                              </strong>
                            </>
                          )}
                        </div>
                        <LatestCommit repo={repo} rev={rev} />
                        {kind === "Tree" ? (
                          <Directory
                            key={`${rev}:${path}`}
                            repo={repo}
                            rev={rev}
                            path={path}
                            onEntry={selectEntry}
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
                          />
                        )}
                      </div>
                    </div>
                  )}
                </Suspense>
              </>
            ) : null
          }
        </Result>
      </div>
    </>
  );
}
function LatestCommit({ repo, rev }: { repo: Repository; rev: string }) {
  const state = useRequest<Commit>(endpoint(repo, "commit", { rev }));
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
            href={repoHref(repo, { view: "commit", rev })}
          >
            {commit.message.split("\n")[0]}
          </Link>
          <Link className="oid" href={repoHref(repo, { view: "commit", rev })}>
            {short(commit.oid)}
          </Link>
          <span className="muted commit-date">
            {date(commit.author_seconds)}
          </span>
        </div>
      )}
    </Result>
  );
}
