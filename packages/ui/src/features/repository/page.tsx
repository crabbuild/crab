import { RepositoryWatch } from "@/features/notifications/preferences";
import { PaneResizer } from "@/components/layout/pane-resizer";
import { useWorkspaceHeight } from "@/components/layout/use-workspace-height";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { DiscussionWorkspace } from "@/components/discussion/workspace";
import type { OrganizationView, Session } from "@/lib/contracts";
import { GitImport } from "@/features/imports";
import {
  displayHex,
  endpoint,
  navigate,
  parentHex,
  useRequest,
  type Entry,
  type Ref,
  type Refs,
  type Repository,
} from "@/lib/api";
import { type CodeThemes } from "@/lib/code-theme";
import {
  repoHref,
  repositoryRef,
  repositoryRoute,
  revisionAddress,
} from "@/lib/repository-route";
import {
  ArchiveIcon,
  CodeIcon,
  GearIcon,
  GitBranchIcon,
  GitPullRequestIcon,
  HistoryIcon,
  IssueOpenedIcon,
  SidebarCollapseIcon,
  TagIcon,
  ProjectIcon,
  RepoForkedIcon,
  SearchIcon,
  ShieldLockIcon,
} from "@primer/octicons-react";
import {
  Suspense,
  lazy,
  useEffect,
  useLayoutEffect,
  useState,
  type CSSProperties,
} from "react";
import { CreateFile, DeleteFile, EditFile, UploadFiles } from "./files/editor";
import { FileNavigation } from "./files/navigation";
import { LatestCommit } from "./latest-commit";
import { ForkSync } from "./fork-sync";
import { RepositoryRefControls, RepositoryToolbar } from "./toolbar";
import { TopicLinks } from "./topic-links";
import { RecentPushes } from "@/features/pull-requests/recent-pushes";

const RepositoryTree = lazy(() =>
  import("./tree").then((module) => ({ default: module.RepositoryTree })),
);
const Directory = lazy(() =>
  import("./browse").then((module) => ({ default: module.Directory })),
);
const FileView = lazy(() =>
  import("./files").then((module) => ({ default: module.FileView })),
);
const History = lazy(() =>
  import("./browse").then((module) => ({ default: module.History })),
);
const CommitView = lazy(() =>
  import("./commits/detail").then((module) => ({ default: module.CommitView })),
);
const Issues = lazy(() =>
  import("@/features/issues").then((module) => ({ default: module.Issues })),
);
const PullRequests = lazy(() =>
  import("@/features/pull-requests").then((module) => ({
    default: module.PullRequests,
  })),
);
const ProjectsPage = lazy(() => import("@/features/projects"));
const ActivityPage = lazy(() => import("@/features/activity"));
const RepositorySecurity = lazy(() => import("./security"));
const MilestonesPage = lazy(() => import("@/features/milestones"));
const LabelsPage = lazy(() =>
  import("@/features/labels").then((module) => ({
    default: module.LabelsPage,
  })),
);
const RefsPage = lazy(() =>
  import("./refs").then((module) => ({ default: module.RefsPage })),
);
const Releases = lazy(() =>
  import("@/features/releases").then((module) => ({
    default: module.Releases,
  })),
);
const RepositoryRulesets = lazy(() =>
  import("@/features/settings/rulesets").then((module) => ({
    default: module.RepositoryRulesets,
  })),
);
const RepositoryProperties = lazy(() =>
  import("@/features/organizations/repository-properties").then((module) => ({
    default: module.RepositoryProperties,
  })),
);
const Settings = lazy(() =>
  import("@/features/settings").then((module) => ({
    default: module.Settings,
  })),
);
const RepositoryCodeSearch = lazy(() => import("./code-search"));
const ForkComparison = lazy(() => import("./fork-compare"));
const BrowseRepositories = lazy(() =>
  import("./catalog-browse").then((module) => ({
    default: module.BrowseRepositories,
  })),
);
const DEFAULT_TREE_PANE_WIDTH = 320;
const MIN_TREE_PANE_WIDTH = 240;
const MAX_TREE_PANE_WIDTH = 640;
const TREE_PANE_KEYBOARD_STEP = 24;

function clampTreePaneWidth(width: number, max = MAX_TREE_PANE_WIDTH) {
  return Math.min(max, Math.max(MIN_TREE_PANE_WIDTH, Math.round(width)));
}

export function RepositoryPage({
  repo,
  ownership,
  url,
  theme,
  codeThemes,
  csrf,
  identity,
  account,
  onRepositoryChanged,
}: {
  repo: Repository;
  ownership?: OrganizationView;
  url: URL;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  csrf: string;
  identity: Session["user"];
  account: string | null;
  onRepositoryChanged: () => void;
}) {
  const route = repositoryRoute(url);
  const view = route?.view ?? "code";
  const publicReader = repo.public_read_only === true;
  const publicView =
    [
      "code",
      "blame",
      "commit",
      "commits",
      "branches",
      "tags",
      "code-search",
      "forks",
      "labels",
      "milestones",
    ].includes(view) ||
    (view === "properties" && !!repo.organization_id) ||
    (view === "issues" && route?.issue !== "new") ||
    (view === "pulls" && route?.pull !== "new" && !route?.action);
  const historyBase = route?.since;
  const canWrite = repo.access === "write" && !repo.archived;
  const organizationId = repo.organization_id;
  const forkSource = repo.forked_from;
  const forkNetwork = repo.fork
    ? forkSource
    : organizationId || repo.visibility === "public"
      ? repo
      : undefined;
  const canCreateFork = repo.can_fork && identity && !repo.archived;
  const refs = useRequest<Refs>(
    view === "projects" ||
      view === "issues" ||
      view === "labels" ||
      view === "milestones" ||
      view === "properties" ||
      view === "activity" ||
      view === "forks"
      ? null
      : endpoint(repo, "refs"),
  );
  const [createdRefs, setCreatedRefs] = useState<Ref[]>([]);
  const [deletedRefs, setDeletedRefs] = useState<string[]>([]);
  const [syncedBranch, setSyncedBranch] = useState<string>();
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
  const creatingFile = view === "create";
  const [showTree, setShowTree] = useState(
    creatingFile ? matchMedia("(min-width: 641px)").matches : Boolean(path),
  );
  const [searchFocusRequest, setSearchFocusRequest] = useState(0);
  const [treePaneWidth, setTreePaneWidth] = useState(DEFAULT_TREE_PANE_WIDTH);
  const [treePaneMax, setTreePaneMax] = useState(MAX_TREE_PANE_WIDTH);
  const [fileLayout, setFileLayout] = useState<HTMLDivElement | null>(null);
  // Code and Blame share one file workspace. Changing presentation must not
  // reopen an explorer the reader just closed.
  useEffect(
    () =>
      setShowTree(
        creatingFile ? matchMedia("(min-width: 641px)").matches : Boolean(path),
      ),
    [path, creatingFile],
  );
  useLayoutEffect(() => {
    const layout = fileLayout;
    if (!layout || !showTree) return;
    const updateLimit = () => {
      if (matchMedia("(max-width: 640px)").matches) return;
      // Suspense can keep this workspace hidden while file tools load.
      // Its zero width must not overwrite the reader's explorer size.
      const width = layout.clientWidth;
      if (width === 0) return;
      const max = Math.max(
        MIN_TREE_PANE_WIDTH,
        Math.min(MAX_TREE_PANE_WIDTH, Math.floor(width / 2)),
      );
      setTreePaneMax(max);
      setTreePaneWidth((width) => clampTreePaneWidth(width, max));
    };
    updateLimit();
    const observer = new ResizeObserver(updateLimit);
    observer.observe(layout);
    return () => observer.disconnect();
  }, [fileLayout, showTree]);
  const overview = view === "code" && !path && !showTree;
  const fileWorkspace =
    (view === "code" || view === "blame" || view === "create") && !overview;
  useWorkspaceHeight(fileWorkspace ? fileLayout : null);
  const issuesView =
    view === "issues" || view === "labels" || view === "milestones";
  const discussionNavigation =
    view === "labels" ||
    view === "milestones" ||
    (view === "issues" && !route?.issue) ||
    (view === "pulls" && !route?.pull);
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
        "X-Crab-Repository-Id": repo.id,
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
  async function deleteBranch(
    name: string,
    expectedOid: string,
    retarget: import("./refs").BranchRetarget | null,
    requestId: string,
  ) {
    const path = retarget ? `pulls/${retarget.pull}/head-branch` : "branches";
    const response = await fetch(endpoint(repo, path), {
      method: retarget ? "POST" : "DELETE",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
        "X-CSRF-Token": csrf,
        "X-Crab-Repository-Id": repo.id,
      },
      body: JSON.stringify(
        retarget
          ? {
              action: "delete",
              request_id: requestId,
              version: retarget.version,
              repository_id: repo.id,
              head_oid: expectedOid,
              retarget_token: retarget.token,
            }
          : {
              name: name.replace(/^refs\/heads\//, ""),
              expected_oid: expectedOid,
            },
      ),
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
  const fileNavigation = visibleRefs && rev && !overview && (
    <FileNavigation
      repo={repo}
      refs={visibleRefs}
      revision={selected?.name ?? revision ?? rev}
      archiveRevision={rev}
      rev={navigationRevision ?? rev}
      path={path}
      kind={kind}
      view={view}
      treeOpen={showTree}
      onOpenTree={() => setShowTree(true)}
      onSearch={focusFileSearch}
    />
  );
  const latestCommit = rev && (
    <LatestCommit
      revision={navigationRevision ?? rev}
      repo={repo}
      rev={rev}
      path={path}
      kind={kind}
    />
  );
  const showWatch = Boolean(identity && !publicReader);
  const showCodeSearch = view !== "code-search";
  const showViewForks = Boolean(forkNetwork && view !== "forks");
  const hasHeaderActions = showWatch || showCodeSearch || showViewForks;
  return (
    <>
      <div className="repo-header flex flex-col lg:flex-row lg:items-center lg:gap-3">
        <nav aria-label="Repository" className="min-w-0 lg:flex-1">
          {[
            {
              view: "code",
              label: "Code",
              icon: CodeIcon,
              active:
                !issuesView &&
                view !== "pulls" &&
                view !== "settings" &&
                view !== "activity" &&
                view !== "security" &&
                view !== "properties" &&
                view !== "projects",
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
            ...(repo.organization_id
              ? [
                  {
                    view: "properties",
                    label: "Properties",
                    icon: TagIcon,
                    active: view === "properties",
                  },
                ]
              : []),
            {
              view: "projects",
              label: "Projects",
              icon: ProjectIcon,
              active: view === "projects",
            },
            {
              view: "activity",
              label: "Activity",
              icon: HistoryIcon,
              active: view === "activity",
            },
            {
              view: "security",
              label: "Security",
              icon: ShieldLockIcon,
              active: view === "security",
            },
            ...(repo.can_admin || repo.can_maintain
              ? [
                  {
                    view: "settings",
                    label: "Settings",
                    icon: GearIcon,
                    active: view === "settings",
                  },
                ]
              : []),
          ]
            .filter(
              (tab) =>
                !publicReader ||
                ["code", "issues", "pulls", "properties"].includes(tab.view),
            )
            .map(({ view: tab, label, icon: Icon, active }) => (
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
        {hasHeaderActions && (
          <div className="repo-header-actions flex shrink-0 flex-wrap items-center justify-end gap-2 border-t border-border/70 py-2 lg:flex-nowrap lg:border-t-0 lg:py-0">
            {showWatch && (
              <RepositoryWatch key={repo.id} repo={repo} csrf={csrf} />
            )}
            {showCodeSearch && (
              <Button asChild variant="outline" size="sm" className="shrink-0">
                <Link
                  aria-label="Search code"
                  href={repoHref(repo, { view: "code-search", at: revision })}
                >
                  <SearchIcon data-icon="inline-start" />
                  <span>Search code</span>
                </Link>
              </Button>
            )}
            {showViewForks && forkNetwork && (
              <Button asChild variant="outline" size="sm" className="shrink-0">
                <Link
                  aria-label="View forks"
                  href={repoHref(forkNetwork, { view: "forks" })}
                >
                  <RepoForkedIcon data-icon="inline-start" />
                  <span>View forks</span>
                </Link>
              </Button>
            )}
          </div>
        )}
      </div>
      {overview && canCreateFork && (
        <div className="flex flex-wrap items-start justify-end gap-2 px-4 py-2">
          <GitImport
            key={`${identity.issuer}:${identity.subject}:${repo.id}`}
            account={JSON.stringify([identity.issuer, identity.subject])}
            organization={
              organizationId
                ? {
                    id: organizationId,
                    name: ownership?.organization.name ?? repo.owner,
                  }
                : undefined
            }
            fork={repo}
            csrf={csrf}
            onImported={(repository) => {
              onRepositoryChanged();
              navigate(repoHref(repository));
            }}
          />
        </div>
      )}
      {repo.fork && (
        <div className="flex flex-wrap items-center justify-between gap-2 px-4 py-2 text-sm text-muted-foreground">
          {forkSource ? (
            <span>
              Forked from{" "}
              <Link
                href={repoHref(forkSource)}
                className="underline underline-offset-4"
              >
                {forkSource.name}
              </Link>
            </span>
          ) : (
            <span>Private fork</span>
          )}
          {overview && forkSource && (
            <div className="flex flex-wrap items-center gap-2">
              <Button asChild variant="outline" size="sm">
                <Link
                  href={repoHref(repo, {
                    view: "fork-compare",
                    rev: branch?.name,
                  })}
                >
                  Compare with upstream
                </Link>
              </Button>
              {canWrite && branch && (
                <>
                  {syncedBranch === branch.name && (
                    <span role="status">Branch updated.</span>
                  )}
                  <ForkSync
                    key={`${repo.id}:${branch.name}`}
                    repo={repo}
                    branch={branch.name}
                    csrf={csrf}
                    onUpdated={() => {
                      setSyncedBranch(branch.name);
                      setCreatedRefs((current) =>
                        current.filter((ref) => ref.name !== branch.name),
                      );
                      refs.retry();
                    }}
                  />
                </>
              )}
            </div>
          )}
        </div>
      )}
      {repo.archived && (
        <div className="archived-banner" role="status">
          <ArchiveIcon size={16} />
          <span>This repository was archived and is read-only.</span>
        </div>
      )}
      {!publicReader &&
        !repo.archived &&
        (!repo.fork || (forkSource && canWrite)) &&
        (overview || (view === "pulls" && !route?.pull && !route?.action)) && (
          <RecentPushes
            key={JSON.stringify([
              repo.id,
              view,
              identity?.issuer,
              identity?.subject,
            ])}
            repo={repo}
          />
        )}
      <div
        className={`repo-body${view === "projects" ? " repo-project-workspace" : ""}${overview ? " repo-overview" : ""}${fileWorkspace ? " repo-file-workspace" : ""}${discussionNavigation ? " repo-discussion-workspace" : ""}${view === "settings" ? " repo-settings-workspace" : ""}`}
      >
        {publicReader && !publicView ? (
          <div className="notice">
            <h2>Repository access required</h2>
            <Link href={repoHref(repo)}>Browse the public code</Link>
          </div>
        ) : view === "forks" ? (
          <section
            aria-labelledby="repository-forks"
            className="mx-auto grid w-full max-w-5xl gap-4"
          >
            <div className="flex flex-wrap items-center justify-between gap-2">
              <h2 id="repository-forks" className="text-xl font-semibold">
                Forks
              </h2>
              <Button asChild variant="outline" size="sm">
                <Link href={repoHref(repo)}>Back to code</Link>
              </Button>
            </div>
            <Suspense
              fallback={
                <p role="status" className="notice">
                  Loading forks…
                </p>
              }
            >
              <BrowseRepositories
                url={url}
                scope={{ kind: "forks", repository: repo }}
              />
            </Suspense>
          </section>
        ) : view === "fork-compare" ? (
          forkSource ? (
            <Result state={visibleRefState} timingLabel="Fork references">
              {(data) => (
                <Suspense
                  fallback={
                    <div className="notice" role="status">
                      <Spinner /> Loading comparison…
                    </div>
                  }
                >
                  <ForkComparison
                    key={`${repo.id}:${route?.head ?? ""}`}
                    repo={repo}
                    source={forkSource}
                    refs={data}
                    requestedHead={route?.head}
                    csrf={csrf}
                    url={url}
                    theme={theme}
                    codeThemes={codeThemes}
                  />
                </Suspense>
              )}
            </Result>
          ) : (
            <div className="notice error" role="alert">
              {repo.fork
                ? "Upstream repository access is required to compare forks."
                : "This repository has no upstream fork source."}
            </div>
          )
        ) : view === "projects" ? (
          <Suspense
            fallback={
              <div className="notice" role="status">
                <Spinner />
                Loading projects…
              </div>
            }
          >
            <ProjectsPage repo={repo} url={url} project={route?.project} />
          </Suspense>
        ) : view === "activity" ? (
          <Suspense
            fallback={
              <div className="notice" role="status">
                <Spinner /> Loading activity…
              </div>
            }
          >
            <ActivityPage repo={repo} />
          </Suspense>
        ) : view === "security" ? (
          <Result state={visibleRefState} timingLabel="Repository references">
            {(data) => (
              <Suspense
                fallback={
                  <div className="notice" role="status">
                    <Spinner /> Loading code scanning…
                  </div>
                }
              >
                <RepositorySecurity
                  key={repo.id}
                  repo={repo}
                  refs={data}
                  url={url}
                />
              </Suspense>
            )}
          </Result>
        ) : view === "rulesets" ? (
          <Suspense
            fallback={
              <div className="notice" role="status">
                <Spinner /> Loading rulesets…
              </div>
            }
          >
            <RepositoryRulesets key={repo.id} repo={repo} />
          </Suspense>
        ) : view === "properties" && repo.organization_id ? (
          <section className="settings-section grid gap-4 p-4">
            <h2 className="text-lg font-semibold">Repository properties</h2>
            <Suspense
              fallback={
                <div role="status">
                  <Spinner /> Loading properties…
                </div>
              }
            >
              <RepositoryProperties key={repo.id} repo={repo} csrf={csrf} />
            </Suspense>
          </section>
        ) : view === "settings" ? (
          repo.can_admin || repo.can_maintain ? (
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
                    ownership={ownership}
                    refs={data}
                    csrf={csrf}
                    section={
                      route?.section === "branches" ||
                      route?.section === "members" ||
                      route?.section === "integrations" ||
                      route?.section === "webhooks" ||
                      route?.section === "deploy-tokens" ||
                      route?.section === "rulesets" ||
                      route?.section === "mentions" ||
                      route?.section === "team-access"
                        ? route.section
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
              Maintain access is required to view repository settings.
            </div>
          )
        ) : issuesView ? (
          <DiscussionWorkspace
            repo={repo}
            url={url}
            view={view}
            showNavigation={discussionNavigation}
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
          </DiscussionWorkspace>
        ) : view === "pulls" ? (
          <DiscussionWorkspace
            repo={repo}
            url={url}
            view="pulls"
            showNavigation={discussionNavigation}
          >
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
                    account={account}
                    theme={theme}
                    codeThemes={codeThemes}
                  />
                </Suspense>
              )}
            </Result>
          </DiscussionWorkspace>
        ) : (
          <Result state={visibleRefState} timingLabel="Repository references">
            {(data) =>
              view === "code-search" ? (
                <Suspense
                  fallback={
                    <div className="notice" role="status">
                      <Spinner /> Loading code search…
                    </div>
                  }
                >
                  <RepositoryCodeSearch repo={repo} refs={data} url={url} />
                </Suspense>
              ) : view === "releases" ? (
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
                    csrf={csrf}
                    onDeleteBranch={canWrite ? deleteBranch : undefined}
                    onBranchCopied={canWrite ? refs.retry : undefined}
                  />
                </Suspense>
              ) : !data.refs.length ? (
                <div className="notice">
                  <h2>This repository is empty</h2>
                  <p>Push an initial commit with Crab to start browsing.</p>
                  {overview && <TopicLinks names={repo.topics} />}
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
                    ) : view === "edit" && canChangeFile ? (
                      <EditFile
                        repo={repo}
                        branch={branch.name}
                        expectedHead={branch.oid}
                        pathHex={path}
                        csrf={csrf}
                        theme={theme}
                        codeThemes={codeThemes}
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
                          key={`${rev}:${path}:${historyBase ?? ""}`}
                          repo={repo}
                          rev={rev}
                          base={historyBase}
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
                          ref={setFileLayout}
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
                            {view === "create" ? (
                              canWrite && branch ? (
                                <CreateFile
                                  key={`${branch.name}:${path}`}
                                  repo={repo}
                                  branch={branch.name}
                                  expectedHead={branch.oid}
                                  directoryHex={
                                    kind === "Tree" ? path : parentHex(path)
                                  }
                                  csrf={csrf}
                                  theme={theme}
                                  codeThemes={codeThemes}
                                  onOpenTree={
                                    !showTree
                                      ? () => setShowTree(true)
                                      : undefined
                                  }
                                />
                              ) : (
                                <div className="notice error" role="alert">
                                  Write access to a branch is required to create
                                  files.
                                </div>
                              )
                            ) : (
                              <>
                                {kind !== "Blob" && fileNavigation}
                                {kind === "Submodule" && latestCommit}
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
                                      header={latestCommit}
                                    />
                                  ) : kind === "Submodule" ? (
                                    <div className="notice">
                                      This entry points to a commit in a
                                      submodule.
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
                                      navigation={fileNavigation}
                                      header={latestCommit}
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
                              </>
                            )}
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
                              <TopicLinks names={repo.topics} />
                              {!publicReader && (
                                <div className="about-links">
                                  <Link
                                    href={repoHref(repo, {
                                      view: "activity",
                                    })}
                                  >
                                    <HistoryIcon /> Activity
                                  </Link>
                                  <Link
                                    href={repoHref(repo, { view: "issues" })}
                                  >
                                    <IssueOpenedIcon /> Issues
                                  </Link>
                                  <Link
                                    href={repoHref(repo, { view: "releases" })}
                                  >
                                    <TagIcon /> Releases
                                  </Link>
                                </div>
                              )}
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
