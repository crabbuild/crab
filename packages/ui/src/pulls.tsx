import { useEffect, useState } from "react";
import type { SelectedLineRange } from "@pierre/diffs";
import { ActionList, ActionMenu, Button, TextInput } from "@primer/react";
import {
  CheckCircleFillIcon,
  ChecklistIcon,
  ClockIcon,
  CodeReviewIcon,
  CommentIcon,
  GitCommitIcon,
  GitMergeIcon,
  GitPullRequestClosedIcon,
  GitPullRequestIcon,
  ShieldLockIcon,
  XCircleFillIcon,
} from "@primer/octicons-react";
import {
  endpoint,
  navigate,
  repoHref,
  request,
  useRequest,
  type Commit,
  type Content,
  type Refs,
  type Repository,
  type RepositoryAssignee,
  type RepositoryLabel,
} from "./api";
import {
  ComparisonView,
  type DiffReviewState,
  type InlineDiffThread,
} from "./content";
import { CommitList } from "./browse";
import { CheckRuns } from "./check-runs";
import { changeContent } from "./content-editor";
import { useMutation } from "./discussion-mutations";
import { DiscussionSearch } from "./discussion-search";
import { LabelBadges } from "./discussion-labels";
import { AssigneeAvatars, DiscussionMetadata } from "./discussion-metadata";
import {
  DiscussionMarkdown,
  Editor,
  Failure,
  useSubmission,
} from "./discussion";
import { Link, Result, short } from "./ui";
import type { CodeThemes } from "./code-theme";
import { replaceSelectedLines } from "./pull-review-model";

interface PullComment {
  number: number;
  author: string;
  body: string;
  version: number;
  created_at: number;
  updated_at: number;
  can_edit: boolean;
}

interface PullSummary {
  number: number;
  title: string;
  state: "open" | "closed" | "merged";
  author: string;
  base_ref: string;
  head_ref: string;
  created_at: number;
  updated_at: number;
  labels: RepositoryLabel[];
  assignees: RepositoryAssignee[];
}

interface PullRequest extends PullSummary {
  body: string;
  version: number;
  can_edit: boolean;
  can_label: boolean;
  can_assign: boolean;
  base_oid: string;
  head_oid: string;
  original_base_oid: string | null;
  original_head_oid: string | null;
  can_manage: boolean;
  can_decide: boolean;
  can_merge: boolean;
  branches_available: boolean;
  merge: {
    author: string;
    method: "fast_forward" | "merge_commit";
    commit_oid: string;
    message: string;
    created_at: number;
  } | null;
  merge_pending: {
    request_id: string;
    author: string;
    method: "fast_forward" | "merge_commit";
    pull_version: number;
    base_oid: string;
    head_oid: string;
    message: string;
    created_at: number;
  } | null;
  merge_requirements: {
    protected: boolean;
    required_approvals: number;
    approvals: number;
    changes_requested: number;
    checks_satisfied: boolean;
    checks: Array<{
      context: string;
      state: "error" | "failure" | "pending" | "success" | null;
      description: string | null;
      target_url: string | null;
      author: string | null;
      updated_at: number | null;
      run_id: number | null;
    }>;
    satisfied: boolean;
  };
}

type ReviewState = "commented" | "approved" | "changes_requested";

interface PullReview {
  number: number;
  author: string;
  body: string;
  state: ReviewState;
  commit_oid: string;
  current: boolean;
  version: number;
  created_at: number;
  updated_at: number;
  can_edit: boolean;
}

interface PullReviewThread extends InlineDiffThread {
  path: string;
  base_oid: string;
  head_oid: string;
  old_blob_oid: string | null;
  new_blob_oid: string | null;
  resolved_by: string | null;
  resolved_at: number | null;
  current: boolean;
  version: number;
  created_at: number;
  updated_at: number;
  can_edit: boolean;
  can_reply: boolean;
  can_resolve: boolean;
}

interface PullReviewReply {
  number: number;
  author: string;
  body: string;
  version: number;
  created_at: number;
  updated_at: number;
  can_edit: boolean;
}

interface Page<T> {
  items: T[];
  next: number | null;
}

function usePagedRequest<T>(url: (before?: number) => string) {
  const [before, setBefore] = useState<number>();
  const [items, setItems] = useState<T[]>([]);
  const state = useRequest<Page<T>>(url(before));
  useEffect(() => {
    if (!state.data) return;
    setItems((current) =>
      before === undefined
        ? state.data!.items
        : [...current, ...state.data!.items],
    );
  }, [before, state.data]);
  function retry() {
    setBefore(undefined);
    setItems([]);
    state.retry();
  }
  function loadMore() {
    if (state.data?.next !== null && state.data?.next !== undefined)
      setBefore(state.data.next);
  }
  return {
    ...state,
    items,
    next: state.data?.next ?? null,
    retry,
    loadMore,
  };
}

interface CursorPage<T> {
  items: T[];
  next: string | null;
}

function timestamp(value: number) {
  return new Date(value).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
}

function branch(name: string) {
  return name.startsWith("refs/heads/")
    ? name.slice("refs/heads/".length)
    : name;
}

function PullBadge({ state }: { state: PullRequest["state"] }) {
  const Icon =
    state === "open"
      ? GitPullRequestIcon
      : state === "merged"
        ? GitMergeIcon
        : GitPullRequestClosedIcon;
  return (
    <span className={`pull-state ${state}`} aria-live="polite">
      <Icon />
      {state === "open" ? "Open" : state === "merged" ? "Merged" : "Closed"}
    </span>
  );
}

export function PullRequests({
  repo,
  refs,
  url,
  csrf,
  theme,
  codeThemes,
}: {
  repo: Repository;
  refs: Refs;
  url: URL;
  csrf: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
}) {
  const pull = url.searchParams.get("pull");
  if (pull === "new")
    return repo.archived ? (
      <div className="notice">
        <h2>This repository is read-only</h2>
        <p>Unarchive it before creating a pull request.</p>
        <Link href={repoHref(repo, { view: "pulls" })}>Back to pulls</Link>
      </div>
    ) : (
      <NewPull
        repo={repo}
        refs={refs}
        url={url}
        csrf={csrf}
        theme={theme}
        codeThemes={codeThemes}
      />
    );
  if (pull) {
    const number = Number(pull);
    if (!Number.isSafeInteger(number) || number <= 0)
      return (
        <div className="notice error">
          <h1>Pull request not found</h1>
          <Link href={repoHref(repo, { view: "pulls" })}>Back to pulls</Link>
        </div>
      );
    return (
      <PullDetail
        key={number}
        repo={repo}
        number={number}
        url={url}
        csrf={csrf}
        theme={theme}
        codeThemes={codeThemes}
      />
    );
  }
  return <PullList repo={repo} url={url} />;
}

function PullList({ repo, url }: { repo: Repository; url: URL }) {
  const state = url.searchParams.get("state") ?? "open";
  const before = url.searchParams.get("before") ?? undefined;
  const query = url.searchParams.get("q") ?? "";
  const page = useRequest<Page<PullSummary>>(
    endpoint(repo, "pulls", { state, before, q: query || undefined }),
  );
  return (
    <section className="pulls-page">
      <div className="section-heading">
        <h2>Pulls</h2>
        {!repo.archived && (
          <Button
            variant="primary"
            onClick={() =>
              navigate(repoHref(repo, { view: "pulls", pull: "new" }))
            }
          >
            New pull request
          </Button>
        )}
      </div>
      <p className="muted">Review and discuss changes between branches.</p>
      <DiscussionSearch
        label="Search pulls"
        placeholder="Search titles, descriptions, or authors"
        value={query}
        onSearch={(value) =>
          navigate(
            repoHref(repo, {
              view: "pulls",
              state,
              q: value || undefined,
            }),
          )
        }
      />
      <div className="discussion-list-actions">
        <Link className="button-link" href={repoHref(repo, { view: "labels" })}>
          Labels
        </Link>
      </div>
      <div className="issues-filters">
        <nav aria-label="Pull request state">
          {["open", "closed", "all"].map((value) => (
            <Link
              key={value}
              className={state === value ? "active" : ""}
              aria-current={state === value ? "page" : undefined}
              href={repoHref(repo, {
                view: "pulls",
                state: value,
                q: query || undefined,
              })}
            >
              {value === "all"
                ? "All pulls"
                : `${value[0].toUpperCase()}${value.slice(1)}`}
            </Link>
          ))}
        </nav>
        <Button size="small" onClick={page.retry}>
          Refresh
        </Button>
      </div>
      <Result state={page}>
        {(data) => (
          <>
            {data.items.length ? (
              <ul className="issue-list panel">
                {data.items.map((pull) => (
                  <li key={pull.number}>
                    <span
                      className={`pull-status-icon ${pull.state}`}
                      role="img"
                      aria-label={pull.state}
                    >
                      {pull.state === "open" ? (
                        <GitPullRequestIcon />
                      ) : pull.state === "merged" ? (
                        <GitMergeIcon />
                      ) : (
                        <GitPullRequestClosedIcon />
                      )}
                    </span>
                    <div>
                      <Link
                        className="issue-link"
                        href={repoHref(repo, {
                          view: "pulls",
                          pull: String(pull.number),
                        })}
                      >
                        {pull.title}
                      </Link>
                      <LabelBadges labels={pull.labels} />
                      <p className="muted">
                        #{pull.number} opened {timestamp(pull.created_at)} by{" "}
                        {pull.author}
                      </p>
                      <p className="pull-branches muted">
                        <code>{branch(pull.head_ref)}</code>
                        <span>into</span>
                        <code>{branch(pull.base_ref)}</code>
                      </p>
                    </div>
                    <AssigneeAvatars assignees={pull.assignees} />
                  </li>
                ))}
              </ul>
            ) : (
              <div className="notice issue-empty">
                <GitPullRequestIcon size={32} />
                <h3>
                  {query ? `No pulls match “${query}”` : "No matching pulls"}
                </h3>
                <p>
                  {query
                    ? data.next
                      ? "Try another search or continue to older pulls."
                      : "Try another title, description, or author."
                    : "Compare two branches to start a review."}
                </p>
              </div>
            )}
            <div className="discussion-pagination">
              {before && (
                <Link
                  href={repoHref(repo, {
                    view: "pulls",
                    state,
                    q: query || undefined,
                  })}
                >
                  Newest pulls
                </Link>
              )}
              {data.next && (
                <Link
                  href={repoHref(repo, {
                    view: "pulls",
                    state,
                    q: query || undefined,
                    before: String(data.next),
                  })}
                >
                  Older pulls →
                </Link>
              )}
            </div>
          </>
        )}
      </Result>
    </section>
  );
}

function NewPull({
  repo,
  refs,
  url,
  csrf,
  theme,
  codeThemes,
}: {
  repo: Repository;
  refs: Refs;
  url: URL;
  csrf: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
}) {
  const branches = refs.refs.filter((ref) =>
    ref.name.startsWith("refs/heads/"),
  );
  const requestedBase = url.searchParams.get("base");
  const initialBase =
    branches.find((ref) => ref.name === requestedBase)?.name ??
    refs.head?.name ??
    branches[0]?.name ??
    "";
  const requestedHead = url.searchParams.get("head");
  const [base, setBase] = useState(initialBase);
  const [head, setHead] = useState(
    branches.find((ref) => ref.name === requestedHead)?.name ??
      branches.find((ref) => ref.name !== initialBase)?.name ??
      initialBase,
  );
  const [title, setTitle] = useState(
    (url.searchParams.get("title") ?? "").slice(0, 256),
  );
  const [body, setBody] = useState("");
  const mutation = useMutation(csrf);
  const submission = useSubmission();
  const baseOid = branches.find((ref) => ref.name === base)?.oid;
  const headOid = branches.find((ref) => ref.name === head)?.oid;
  const comparable = Boolean(baseOid && headOid && baseOid !== headOid);
  return (
    <section className="pulls-page pull-compose">
      <Link href={repoHref(repo, { view: "pulls" })}>← Back to pulls</Link>
      <div className="compare-heading">
        <h2>Compare changes</h2>
        <p className="muted">
          Choose a base branch and a different head branch to review.
        </p>
      </div>
      <div className="panel compare-picker">
        <GitMergeIcon />
        <label>
          <span>base:</span>
          <select
            value={base}
            onChange={(event) => setBase(event.target.value)}
          >
            {branches.map((ref) => (
              <option key={ref.name} value={ref.name}>
                {branch(ref.name)}
              </option>
            ))}
          </select>
        </label>
        <span aria-hidden="true">←</span>
        <label>
          <span>compare:</span>
          <select
            value={head}
            onChange={(event) => setHead(event.target.value)}
          >
            {branches.map((ref) => (
              <option key={ref.name} value={ref.name}>
                {branch(ref.name)}
              </option>
            ))}
          </select>
        </label>
      </div>
      {branches.length < 2 ? (
        <div className="notice">
          <h3>Two branches are required</h3>
          <p>Push another branch before creating a pull request.</p>
        </div>
      ) : !comparable ? (
        <div className="notice">
          <h3>Choose branches with different commits</h3>
          <p>The head branch currently has no commit difference to review.</p>
        </div>
      ) : (
        <>
          <form
            className="panel discussion-form pull-form"
            onSubmit={async (event) => {
              event.preventDefault();
              const input = { title, body, base_ref: base, head_ref: head };
              const created = await mutation.run<PullRequest>(
                endpoint(repo, "pulls"),
                "POST",
                { ...input, request_id: submission(input) },
              );
              if (created)
                navigate(
                  repoHref(repo, {
                    view: "pulls",
                    pull: String(created.number),
                  }),
                );
            }}
          >
            <h3>Open a pull request</h3>
            <label htmlFor="pull-title">Title</label>
            <input
              id="pull-title"
              autoFocus
              required
              maxLength={256}
              value={title}
              disabled={mutation.pending}
              onChange={(event) => setTitle(event.target.value)}
            />
            <Editor
              id="pull-body"
              label="Description"
              value={body}
              onChange={setBody}
              disabled={mutation.pending}
            />
            <Failure message={mutation.error} />
            <div className="discussion-actions">
              <Button
                type="submit"
                variant="primary"
                disabled={mutation.pending || !title.trim()}
              >
                {mutation.pending ? "Creating…" : "Create pull request"}
              </Button>
            </div>
          </form>
          <ComparisonView
            repo={repo}
            base={baseOid ?? ""}
            head={headOid ?? ""}
            theme={theme}
            codeThemes={codeThemes}
          />
        </>
      )}
    </section>
  );
}

function PullDetail({
  repo,
  number,
  url,
  csrf,
  theme,
  codeThemes,
}: {
  repo: Repository;
  number: number;
  url: URL;
  csrf: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
}) {
  const path = endpoint(repo, `pulls/${number}`);
  const pull = useRequest<PullRequest>(path);
  const requestedTab = url.searchParams.get("pull_tab");
  const tab =
    requestedTab === "commits" ||
    requestedTab === "files" ||
    requestedTab === "checks"
      ? requestedTab
      : "conversation";
  const requestedCheck = Number(url.searchParams.get("check"));
  const selectedCheck =
    Number.isSafeInteger(requestedCheck) && requestedCheck > 0
      ? requestedCheck
      : undefined;
  const requestedCheckBefore = Number(url.searchParams.get("check_before"));
  const checkBefore =
    Number.isSafeInteger(requestedCheckBefore) && requestedCheckBefore > 0
      ? requestedCheckBefore
      : undefined;
  return (
    <section className="pulls-page pull-detail">
      <Result state={pull}>
        {(data) => (
          <>
            <div className="pull-title">
              <h2>
                {data.title} <span className="muted">#{data.number}</span>
              </h2>
              <div className="pull-summary">
                <PullBadge state={data.state} />
                {data.merge ? (
                  <span>
                    <strong>{data.merge.author}</strong>{" "}
                    {data.merge.method === "merge_commit"
                      ? "created merge commit"
                      : "fast-forwarded"}{" "}
                    <code>{short(data.merge.commit_oid)}</code> into{" "}
                    <code>{branch(data.base_ref)}</code>
                  </span>
                ) : (
                  <span>
                    <strong>{data.author}</strong> wants to merge{" "}
                    <code>{short(data.head_oid)}</code> from{" "}
                    <code>{branch(data.head_ref)}</code> into{" "}
                    <code>{branch(data.base_ref)}</code>
                  </span>
                )}
              </div>
            </div>
            <nav className="pull-tabs" aria-label="Pull request">
              <Link
                className={tab === "conversation" ? "active" : ""}
                aria-current={tab === "conversation" ? "page" : undefined}
                href={repoHref(repo, {
                  view: "pulls",
                  pull: String(number),
                })}
              >
                <CommentIcon /> Conversation
              </Link>
              <Link
                className={tab === "commits" ? "active" : ""}
                aria-current={tab === "commits" ? "page" : undefined}
                href={repoHref(repo, {
                  view: "pulls",
                  pull: String(number),
                  pull_tab: "commits",
                })}
              >
                <GitCommitIcon /> Commits
              </Link>
              <Link
                className={tab === "checks" ? "active" : ""}
                aria-current={tab === "checks" ? "page" : undefined}
                href={repoHref(repo, {
                  view: "pulls",
                  pull: String(number),
                  pull_tab: "checks",
                })}
              >
                <ChecklistIcon /> Checks
              </Link>
              <Link
                className={tab === "files" ? "active" : ""}
                aria-current={tab === "files" ? "page" : undefined}
                href={repoHref(repo, {
                  view: "pulls",
                  pull: String(number),
                  pull_tab: "files",
                })}
              >
                Files changed
              </Link>
            </nav>
            {tab !== "checks" && !data.branches_available && (
              <div className="notice error">
                One of this pull request&apos;s branches no longer exists. The
                original commit IDs remain recorded, but the live comparison is
                unavailable.
              </div>
            )}
            {tab === "checks" ? (
              <CheckRuns
                repo={repo}
                pull={number}
                oid={data.head_oid}
                selected={selectedCheck}
                before={checkBefore}
              />
            ) : tab === "commits" ? (
              data.branches_available ? (
                <PullCommits repo={repo} pull={data} url={url} />
              ) : null
            ) : tab === "files" ? (
              data.branches_available ? (
                <>
                  <PullReviewThreads
                    repo={repo}
                    pull={data}
                    csrf={csrf}
                    theme={theme}
                    codeThemes={codeThemes}
                    refresh={pull.retry}
                  />
                  {data.state === "open" && !repo.archived && (
                    <ReviewForm
                      repo={repo}
                      pull={data}
                      csrf={csrf}
                      refresh={pull.retry}
                    />
                  )}
                </>
              ) : null
            ) : (
              <div className="discussion-detail-layout">
                <PullConversation
                  repo={repo}
                  pull={data}
                  csrf={csrf}
                  refresh={pull.retry}
                />
                <DiscussionMetadata
                  repo={repo}
                  assignees={data.assignees}
                  labels={data.labels}
                  canAssign={data.can_assign && !repo.archived}
                  canLabel={data.can_label && !repo.archived}
                  version={data.version}
                  path={`pulls/${number}`}
                  csrf={csrf}
                  onSaved={pull.retry}
                />
              </div>
            )}
          </>
        )}
      </Result>
    </section>
  );
}

type ThreadSelection = {
  pathHex: string;
  range: SelectedLineRange;
};

function PullReviewThreads({
  repo,
  pull,
  csrf,
  theme,
  codeThemes,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  refresh: () => void;
}) {
  const path = (before?: number) =>
    endpoint(repo, `pulls/${pull.number}/threads`, {
      outdated: "false",
      before: before?.toString(),
    });
  const outdatedPath = (before?: number) =>
    endpoint(repo, `pulls/${pull.number}/threads`, {
      outdated: "true",
      before: before?.toString(),
    });
  const threads = usePagedRequest<PullReviewThread>(path);
  const outdatedThreads = usePagedRequest<PullReviewThread>(outdatedPath);
  const [selection, setSelection] = useState<ThreadSelection>();
  const [body, setBody] = useState("");
  const [suggestion, setSuggestion] = useState("");
  const [suggestionEnabled, setSuggestionEnabled] = useState(false);
  const mutation = useMutation(csrf);
  const submission = useSubmission();
  function clearSelection() {
    setSelection(undefined);
    setBody("");
    setSuggestion("");
    setSuggestionEnabled(false);
  }
  const review: DiffReviewState = {
    threads: threads.items,
    onLineSelected(pathHex, range) {
      if (!range || range.endSide) {
        clearSelection();
        return;
      }
      setSelection({ pathHex, range });
      setBody("");
      setSuggestion("");
      setSuggestionEnabled(false);
    },
    onThreadSelected(thread) {
      const node = document.getElementById(`review-thread-${thread.number}`);
      node?.scrollIntoView({ behavior: "smooth", block: "center" });
    },
    renderAnnotation(thread) {
      return (
        <span>
          #{thread.number} {thread.resolved ? "Resolved" : "Review thread"}
        </span>
      );
    },
  };
  const selectedSide = selection?.range.side === "deletions" ? "old" : "new";
  const selectedStart = selection
    ? Math.min(selection.range.start, selection.range.end)
    : 0;
  const selectedEnd = selection
    ? Math.max(selection.range.start, selection.range.end)
    : 0;
  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (!selection) return;
    const input = {
      body,
      suggested_text:
        selectedSide === "new" && suggestionEnabled ? suggestion : null,
      base_oid: pull.base_oid,
      head_oid: pull.head_oid,
      path_hex: selection.pathHex,
      side: selectedSide,
      start_line: selectedStart,
      end_line: selectedEnd,
    };
    const created = await mutation.run<PullReviewThread>(path(), "POST", {
      ...input,
      request_id: submission(input),
    });
    if (created) {
      clearSelection();
      threads.retry();
      outdatedThreads.retry();
    }
  }
  return (
    <section className="pull-review-threads" aria-label="Inline review threads">
      <ComparisonView
        repo={repo}
        base={pull.base_oid}
        head={pull.head_oid}
        theme={theme}
        codeThemes={codeThemes}
        review={review}
      />
      {selection && pull.state === "open" && !repo.archived && (
        <form
          className="panel discussion-form inline-review-form"
          onSubmit={submit}
          aria-labelledby="inline-review-composer-heading"
        >
          <h3 id="inline-review-composer-heading">Comment on selected lines</h3>
          <p className="muted">
            {selectedSide === "old" ? "Old" : "New"} lines {selectedStart}–
            {selectedEnd} · <code>{selection.pathHex}</code>
          </p>
          <Editor
            id="inline-review-body"
            label="Comment"
            value={body}
            onChange={setBody}
            disabled={mutation.pending}
            required
          />
          {selectedSide === "new" && (
            <div className="inline-review-suggestion">
              <label>
                <input
                  type="checkbox"
                  checked={suggestionEnabled}
                  disabled={mutation.pending}
                  onChange={(event) =>
                    setSuggestionEnabled(event.target.checked)
                  }
                />{" "}
                Include suggested replacement
              </label>
              {suggestionEnabled && (
                <textarea
                  aria-label="Suggested replacement"
                  value={suggestion}
                  rows={4}
                  maxLength={65536}
                  disabled={mutation.pending}
                  onChange={(event) => setSuggestion(event.target.value)}
                />
              )}
            </div>
          )}
          <Failure message={mutation.error} />
          <div className="discussion-actions">
            <Button
              type="submit"
              variant="primary"
              disabled={mutation.pending || !body.trim()}
            >
              {mutation.pending ? "Posting…" : "Comment"}
            </Button>
            <Button type="button" onClick={clearSelection}>
              Cancel
            </Button>
          </div>
        </form>
      )}
      <Result state={threads}>
        {() => (
          <div className="inline-review-list">
            <div className="section-heading">
              <div>
                <h3 id="inline-review-heading">Inline review threads</h3>
                <p className="muted">
                  Select lines in the diff to start a thread. Outdated threads
                  remain visible for context.
                </p>
              </div>
              <Button size="small" onClick={threads.retry}>
                Refresh
              </Button>
            </div>
            {threads.items.length ? (
              threads.items.map((thread) => (
                <ReviewThreadCard
                  key={thread.number}
                  repo={repo}
                  pull={pull}
                  thread={thread}
                  csrf={csrf}
                  refresh={() => {
                    threads.retry();
                    outdatedThreads.retry();
                    refresh();
                  }}
                />
              ))
            ) : (
              <p className="muted">No inline review threads yet.</p>
            )}
            {threads.next !== null && (
              <Button size="small" onClick={threads.loadMore}>
                Load more threads
              </Button>
            )}
          </div>
        )}
      </Result>
      <details className="inline-review-outdated">
        <summary>
          Outdated conversations ({outdatedThreads.items.length})
        </summary>
        <Result state={outdatedThreads}>
          {() =>
            outdatedThreads.items.length ? (
              <div className="inline-review-list">
                {outdatedThreads.items.map((thread) => (
                  <ReviewThreadCard
                    key={thread.number}
                    repo={repo}
                    pull={pull}
                    thread={thread}
                    csrf={csrf}
                    refresh={() => {
                      threads.retry();
                      outdatedThreads.retry();
                      refresh();
                    }}
                  />
                ))}
                {outdatedThreads.next !== null && (
                  <Button size="small" onClick={outdatedThreads.loadMore}>
                    Load more outdated conversations
                  </Button>
                )}
              </div>
            ) : (
              <p className="muted">No outdated conversations.</p>
            )
          }
        </Result>
      </details>
    </section>
  );
}

function ReviewThreadCard({
  repo,
  pull,
  thread,
  csrf,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  thread: PullReviewThread;
  csrf: string;
  refresh: () => void;
}) {
  const mutation = useMutation(csrf);
  const [editing, setEditing] = useState(false);
  const [editBody, setEditBody] = useState(thread.body);
  const location = `${thread.path}:${thread.start_line}${thread.end_line === thread.start_line ? "" : `–${thread.end_line}`}`;
  async function saveEdit(event: React.FormEvent) {
    event.preventDefault();
    const updated = await mutation.run<PullReviewThread>(
      endpoint(repo, `pulls/${pull.number}/threads/${thread.number}`),
      "PATCH",
      { version: thread.version, body: editBody },
    );
    if (updated) {
      setEditing(false);
      refresh();
    }
  }
  async function toggleResolved() {
    const updated = await mutation.run<PullReviewThread>(
      endpoint(repo, `pulls/${pull.number}/threads/${thread.number}`),
      "PATCH",
      { version: thread.version, resolved: !thread.resolved },
    );
    if (updated) refresh();
  }
  return (
    <article
      id={`review-thread-${thread.number}`}
      className={`panel review-thread-card${thread.outdated ? " outdated" : ""}${thread.resolved ? " resolved" : ""}`}
    >
      <details
        className="review-thread-details"
        open={!thread.resolved || undefined}
      >
        <summary className="review-thread-summary">
          <span>
            <strong>{thread.author}</strong>{" "}
            <span className="muted">commented on {location}</span>
          </span>
          {thread.outdated && (
            <span className="review-thread-badge">Outdated</span>
          )}
          {thread.resolved && (
            <span className="review-thread-badge">Resolved</span>
          )}
        </summary>
        <div className="review-thread-content">
          {editing ? (
            <form className="review-thread-edit" onSubmit={saveEdit}>
              <Editor
                id={`thread-${thread.number}-edit`}
                label="Edit comment"
                value={editBody}
                onChange={setEditBody}
                disabled={mutation.pending}
                required
                autoFocus
              />
              <div className="discussion-actions">
                <Button
                  size="small"
                  variant="primary"
                  type="submit"
                  disabled={mutation.pending || !editBody.trim()}
                >
                  {mutation.pending ? "Saving…" : "Save"}
                </Button>
                <Button
                  size="small"
                  type="button"
                  disabled={mutation.pending}
                  onClick={() => {
                    setEditBody(thread.body);
                    setEditing(false);
                  }}
                >
                  Cancel
                </Button>
              </div>
            </form>
          ) : (
            <DiscussionMarkdown>{thread.body}</DiscussionMarkdown>
          )}
          {thread.suggested_text !== null &&
            !thread.outdated &&
            !thread.vanished &&
            !thread.resolved &&
            thread.side === "new" &&
            repo.access === "write" && (
              <SuggestionAction
                repo={repo}
                pull={pull}
                thread={thread}
                csrf={csrf}
                refresh={refresh}
              />
            )}
          <div className="discussion-actions">
            {thread.can_edit && !repo.archived && !editing && (
              <Button
                size="small"
                disabled={mutation.pending}
                onClick={() => {
                  setEditBody(thread.body);
                  setEditing(true);
                }}
              >
                Edit
              </Button>
            )}
            {thread.can_resolve && (
              <Button
                size="small"
                disabled={mutation.pending}
                onClick={toggleResolved}
              >
                {thread.resolved ? "Reopen" : "Resolve"}
              </Button>
            )}
          </div>
          {thread.outdated && (
            <p className="muted review-thread-warning">
              This comment is attached to an earlier version of the file. The
              path or anchored lines may no longer exist.
            </p>
          )}
          <ReviewReplies repo={repo} pull={pull} thread={thread} csrf={csrf} />
          <Failure message={mutation.error} />
        </div>
      </details>
    </article>
  );
}

function SuggestionAction({
  repo,
  pull,
  thread,
  csrf,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  thread: PullReviewThread;
  csrf: string;
  refresh: () => void;
}) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string>();
  async function apply() {
    if (thread.suggested_text === null || !thread.new_blob_oid) return;
    setPending(true);
    setError(undefined);
    try {
      const file = await request<Content>(
        endpoint(repo, "file", {
          rev: pull.head_oid,
          path_hex: thread.path_hex,
        }),
        new AbortController().signal,
      );
      if (
        file.data.text === null ||
        file.data.text_truncated ||
        file.data.classification !== "OrdinaryGit" ||
        file.data.size > 900 * 1024
      )
        throw new Error("The file is not an editable UTF-8 text file");
      const content = replaceSelectedLines(
        file.data.text,
        thread.start_line,
        thread.end_line,
        thread.suggested_text,
      );
      if (new TextEncoder().encode(content).byteLength > 900 * 1024)
        throw new Error("The updated file is larger than 900 KiB");
      await changeContent(repo, csrf, "PATCH", {
        branch: pull.head_ref,
        expected_head: pull.head_oid,
        expected_blob: thread.new_blob_oid,
        path_hex: thread.path_hex,
        content,
        message: `Apply suggestion from review thread #${thread.number}`,
      });
      refresh();
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "The suggestion could not be applied",
      );
    } finally {
      setPending(false);
    }
  }
  return (
    <div className="review-suggestion">
      <pre>{thread.suggested_text}</pre>
      {pull.state === "open" && !repo.archived && repo.access === "write" && (
        <Button
          size="small"
          variant="primary"
          disabled={pending}
          onClick={apply}
        >
          {pending ? "Applying…" : "Apply suggestion"}
        </Button>
      )}
      <Failure message={error} />
    </div>
  );
}

function ReviewReplies({
  repo,
  pull,
  thread,
  csrf,
}: {
  repo: Repository;
  pull: PullRequest;
  thread: PullReviewThread;
  csrf: string;
}) {
  const repliesPath = (before?: number) =>
    endpoint(repo, `pulls/${pull.number}/threads/${thread.number}/replies`, {
      before: before?.toString(),
    });
  const replies = usePagedRequest<PullReviewReply>(repliesPath);
  const [body, setBody] = useState("");
  const mutation = useMutation(csrf);
  const submission = useSubmission();
  async function submit(event: React.FormEvent) {
    event.preventDefault();
    const input = { body };
    const created = await mutation.run<PullReviewReply>(repliesPath(), "POST", {
      ...input,
      request_id: submission(input),
    });
    if (created) {
      setBody("");
      replies.retry();
    }
  }
  return (
    <div className="review-replies">
      <Result state={replies}>
        {() =>
          replies.items.map((reply) => (
            <ReviewReplyItem
              key={reply.number}
              repo={repo}
              pull={pull}
              thread={thread}
              reply={reply}
              csrf={csrf}
              refresh={replies.retry}
            />
          ))
        }
      </Result>
      {replies.next !== null && (
        <Button size="small" onClick={replies.loadMore}>
          Load more replies
        </Button>
      )}
      {thread.can_reply && !repo.archived && (
        <form className="review-reply-form" onSubmit={submit}>
          <Editor
            id={`reply-${thread.number}`}
            label="Reply"
            value={body}
            onChange={setBody}
            disabled={mutation.pending}
            required
          />
          <Failure message={mutation.error} />
          <Button
            size="small"
            type="submit"
            disabled={mutation.pending || !body.trim()}
          >
            {mutation.pending ? "Replying…" : "Reply"}
          </Button>
        </form>
      )}
    </div>
  );
}

function ReviewReplyItem({
  repo,
  pull,
  thread,
  reply,
  csrf,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  thread: PullReviewThread;
  reply: PullReviewReply;
  csrf: string;
  refresh: () => void;
}) {
  const mutation = useMutation(csrf);
  const [editing, setEditing] = useState(false);
  const [body, setBody] = useState(reply.body);
  async function save(event: React.FormEvent) {
    event.preventDefault();
    const updated = await mutation.run<PullReviewReply>(
      endpoint(
        repo,
        `pulls/${pull.number}/threads/${thread.number}/replies/${reply.number}`,
      ),
      "PATCH",
      { version: reply.version, body },
    );
    if (updated) {
      setBody(updated.body);
      setEditing(false);
      refresh();
    }
  }
  return (
    <div className="review-reply">
      <strong>{reply.author}</strong>
      <span className="muted">replied {timestamp(reply.created_at)}</span>
      {editing ? (
        <form className="review-reply-edit" onSubmit={save}>
          <Editor
            id={`reply-${thread.number}-${reply.number}-edit`}
            label="Edit reply"
            value={body}
            onChange={setBody}
            disabled={mutation.pending}
            required
            autoFocus
          />
          <div className="discussion-actions">
            <Button
              size="small"
              variant="primary"
              type="submit"
              disabled={mutation.pending || !body.trim()}
            >
              {mutation.pending ? "Saving…" : "Save"}
            </Button>
            <Button
              size="small"
              type="button"
              disabled={mutation.pending}
              onClick={() => {
                setBody(reply.body);
                setEditing(false);
              }}
            >
              Cancel
            </Button>
          </div>
        </form>
      ) : (
        <>
          <DiscussionMarkdown>{body}</DiscussionMarkdown>
          {reply.can_edit && !repo.archived && (
            <Button
              size="small"
              disabled={mutation.pending}
              onClick={() => setEditing(true)}
            >
              Edit
            </Button>
          )}
        </>
      )}
      <Failure message={mutation.error} />
    </div>
  );
}

function PullCommits({
  repo,
  pull,
  url,
}: {
  repo: Repository;
  pull: PullRequest;
  url: URL;
}) {
  const cursor = url.searchParams.get("commit_after") ?? undefined;
  const commits = useRequest<CursorPage<Commit>>(
    endpoint(repo, "commits", {
      rev: pull.head_oid,
      base: pull.base_oid,
      limit: "50",
      cursor,
    }),
  );
  return (
    <section className="pull-commits" aria-labelledby="pull-commits-heading">
      <div className="section-heading">
        <div>
          <h3 id="pull-commits-heading">Commits</h3>
          <p className="muted">
            Changes reachable from <code>{branch(pull.head_ref)}</code> and not
            from <code>{branch(pull.base_ref)}</code>.
          </p>
        </div>
        <Button size="small" onClick={commits.retry}>
          Refresh
        </Button>
      </div>
      <Result state={commits}>
        {(page) => (
          <>
            {page.items.length ? (
              <section className="panel">
                <CommitList repo={repo} commits={page.items} />
              </section>
            ) : (
              <div className="notice">
                <GitCommitIcon size={24} />
                <strong>No commits to show</strong>
                <p>
                  The head contains no commits that are absent from the base.
                </p>
              </div>
            )}
            <div className="discussion-pagination">
              {cursor && (
                <Link
                  href={repoHref(repo, {
                    view: "pulls",
                    pull: String(pull.number),
                    pull_tab: "commits",
                  })}
                >
                  Newest commits
                </Link>
              )}
              {page.next && (
                <Link
                  href={repoHref(repo, {
                    view: "pulls",
                    pull: String(pull.number),
                    pull_tab: "commits",
                    commit_after: page.next,
                  })}
                >
                  Older commits →
                </Link>
              )}
            </div>
          </>
        )}
      </Result>
    </section>
  );
}

function PullConversation({
  repo,
  pull,
  csrf,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  refresh: () => void;
}) {
  const commentsPath = endpoint(repo, `pulls/${pull.number}/comments`);
  const comments = useRequest<Page<PullComment>>(commentsPath);
  const reviews = useRequest<Page<PullReview>>(
    endpoint(repo, `pulls/${pull.number}/reviews`),
  );
  const [body, setBody] = useState("");
  const commentMutation = useMutation(csrf);
  const stateMutation = useMutation(csrf);
  const submission = useSubmission();
  return (
    <div className="pull-conversation">
      <article className="panel discussion-card">
        <header>
          <strong>{pull.author}</strong>
          <span className="muted">commented {timestamp(pull.created_at)}</span>
        </header>
        <DiscussionMarkdown>{pull.body}</DiscussionMarkdown>
      </article>
      <Result state={comments}>
        {(commentPage) => (
          <Result state={reviews}>
            {(reviewPage) => (
              <PullTimeline
                comments={commentPage.items}
                reviews={reviewPage.items}
              />
            )}
          </Result>
        )}
      </Result>
      {!repo.archived && (
        <form
          className="panel discussion-form"
          onSubmit={async (event) => {
            event.preventDefault();
            const input = { body };
            const created = await commentMutation.run<PullComment>(
              commentsPath,
              "POST",
              { ...input, request_id: submission(input) },
            );
            if (created) {
              setBody("");
              comments.retry();
            }
          }}
        >
          <h3>Join the conversation</h3>
          <Editor
            id="pull-comment"
            label="Comment"
            value={body}
            onChange={setBody}
            disabled={commentMutation.pending}
            required
          />
          <Failure message={commentMutation.error} />
          <div className="discussion-actions">
            <Button
              type="submit"
              variant="primary"
              disabled={commentMutation.pending || !body.trim()}
            >
              {commentMutation.pending ? "Commenting…" : "Comment"}
            </Button>
            {pull.can_manage && (
              <Button
                type="button"
                variant={pull.state === "open" ? "danger" : "default"}
                disabled={stateMutation.pending}
                onClick={async () => {
                  const updated = await stateMutation.run<PullRequest>(
                    endpoint(repo, `pulls/${pull.number}`),
                    "PATCH",
                    {
                      version: pull.version,
                      state: pull.state === "open" ? "closed" : "open",
                    },
                  );
                  if (updated) refresh();
                }}
              >
                {pull.state === "open"
                  ? "Close pull request"
                  : "Reopen pull request"}
              </Button>
            )}
          </div>
          <Failure message={stateMutation.error} />
        </form>
      )}
      {!repo.archived && (
        <div className="notice pull-merge-note">
          <MergePanel repo={repo} pull={pull} csrf={csrf} refresh={refresh} />
        </div>
      )}
    </div>
  );
}

function MergePanel({
  repo,
  pull,
  csrf,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  refresh: () => void;
}) {
  const mutation = useMutation(csrf);
  const submission = useSubmission();
  const pending = pull.merge_pending;
  const [method, setMethod] = useState<"merge_commit" | "fast_forward">(
    "merge_commit",
  );
  const defaultMessage = `Merge pull request #${pull.number} from ${branch(pull.head_ref)}`;
  const [message, setMessage] = useState(defaultMessage);
  const selectedMethod = pending?.method ?? method;
  const requirements = pull.merge_requirements;
  const protection = repo.protected_branches.find(
    (rule) => rule.branch === branch(pull.base_ref),
  );
  const reviewsSatisfied =
    requirements.required_approvals === 0 ||
    (requirements.changes_requested === 0 &&
      requirements.approvals >= requirements.required_approvals);
  if (pull.merge)
    return (
      <>
        <GitMergeIcon className="merge-status-icon" />
        <div>
          <strong>Pull request merged</strong>
          <p>
            {pull.merge.author}{" "}
            {pull.merge.method === "merge_commit"
              ? "created merge commit"
              : "fast-forwarded commit"}{" "}
            <code>{short(pull.merge.commit_oid)}</code> into{" "}
            <code>{branch(pull.base_ref)}</code>{" "}
            {timestamp(pull.merge.created_at)}.
          </p>
        </div>
      </>
    );
  if (pull.state === "closed")
    return (
      <>
        <GitPullRequestClosedIcon />
        <div>
          <strong>This pull request is closed</strong>
          <p>Reopen it before merging these commits.</p>
        </div>
      </>
    );
  if (!pull.branches_available && !pending)
    return (
      <>
        <GitMergeIcon />
        <div>
          <strong>This pull request cannot be merged</strong>
          <p>The base or head branch is unavailable.</p>
        </div>
      </>
    );
  return (
    <>
      <GitMergeIcon />
      <div className="merge-action">
        <strong>
          {pending
            ? `${pending.author} started ${pending.method === "merge_commit" ? "a merge commit" : "a fast-forward merge"}`
            : !requirements.satisfied
              ? "Merging is blocked"
              : "Merge requirements are satisfied"}
        </strong>
        <p>
          Crab verifies ancestry, dependency content, visibility, and the exact
          branch tips again while holding the base ref lock.
        </p>
        {protection && (
          <p className="protected-branch-note">
            <ShieldLockIcon />
            <span>
              <code>{branch(pull.base_ref)}</code> is protected. Direct pushes
              are blocked; this exact head can publish through the pull request
              merge path.
            </span>
          </p>
        )}
        <RequiredChecks
          repo={repo}
          pull={pull}
          requirements={requirements}
          refresh={refresh}
        />
        {pull.can_merge ? (
          <div className="merge-controls">
            {!pending && (
              <>
                <ActionMenu>
                  <ActionMenu.Button>
                    {method === "merge_commit"
                      ? "Create a merge commit"
                      : "Fast-forward only"}
                  </ActionMenu.Button>
                  <ActionMenu.Overlay width="medium">
                    <ActionList selectionVariant="single">
                      <ActionList.Item
                        selected={method === "merge_commit"}
                        onSelect={() => setMethod("merge_commit")}
                      >
                        Create a merge commit
                        <ActionList.Description variant="block">
                          Add all commits with a two-parent merge commit.
                        </ActionList.Description>
                      </ActionList.Item>
                      <ActionList.Item
                        selected={method === "fast_forward"}
                        onSelect={() => setMethod("fast_forward")}
                      >
                        Fast-forward only
                        <ActionList.Description variant="block">
                          Move the base branch only when it is an ancestor.
                        </ActionList.Description>
                      </ActionList.Item>
                    </ActionList>
                  </ActionMenu.Overlay>
                </ActionMenu>
                {method === "merge_commit" && (
                  <TextInput
                    block
                    aria-label="Merge commit message"
                    value={message}
                    maxLength={256}
                    onChange={(event) => setMessage(event.target.value)}
                  />
                )}
              </>
            )}
            <Button
              variant="primary"
              disabled={mutation.pending}
              onClick={async () => {
                const input = pending
                  ? {
                      version: pending.pull_version,
                      method: pending.method,
                      base_oid: pending.base_oid,
                      head_oid: pending.head_oid,
                      message: pending.message,
                    }
                  : {
                      version: pull.version,
                      method: selectedMethod,
                      base_oid: pull.base_oid,
                      head_oid: pull.head_oid,
                      message: selectedMethod === "merge_commit" ? message : "",
                    };
                const merged = await mutation.run<PullRequest>(
                  endpoint(repo, `pulls/${pull.number}/merge`),
                  "POST",
                  {
                    ...input,
                    request_id: pending?.request_id ?? submission(input),
                  },
                );
                if (merged) refresh();
              }}
            >
              {mutation.pending
                ? "Merging…"
                : pending
                  ? "Retry merge"
                  : "Merge pull request"}
            </Button>
          </div>
        ) : repo.access === "write" && !reviewsSatisfied ? (
          <p className="merge-blocked-note">
            <XCircleFillIcon />
            <span>
              {requirements.changes_requested > 0
                ? `${requirements.changes_requested} current change request${requirements.changes_requested === 1 ? "" : "s"} must be resolved.`
                : `${Math.max(0, requirements.required_approvals - requirements.approvals)} more approving review${requirements.required_approvals - requirements.approvals === 1 ? " is" : "s are"} required.`}
            </span>
          </p>
        ) : repo.access === "read" ? (
          <p className="muted">Write access is required to merge.</p>
        ) : null}
        <Failure message={mutation.error} />
      </div>
    </>
  );
}

function RequiredChecks({
  repo,
  pull,
  requirements,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  requirements: PullRequest["merge_requirements"];
  refresh: () => void;
}) {
  if (!requirements.checks.length) return null;
  const unsuccessful = requirements.checks.some(
    (check) => check.state === "error" || check.state === "failure",
  );
  const waiting = requirements.checks.some(
    (check) => check.state === null || check.state === "pending",
  );
  const SummaryIcon = unsuccessful
    ? XCircleFillIcon
    : waiting
      ? ClockIcon
      : CheckCircleFillIcon;
  return (
    <div
      className={`required-checks ${unsuccessful ? "failure" : waiting ? "pending" : "success"}`}
    >
      <div className="required-checks-summary">
        <SummaryIcon />
        <strong>
          {unsuccessful
            ? "Some required checks were not successful"
            : waiting
              ? "Required checks are waiting"
              : "All required checks have passed"}
        </strong>
        <Button size="small" onClick={refresh}>
          Refresh checks
        </Button>
      </div>
      <ul>
        {requirements.checks.map((check) => {
          const CheckIcon =
            check.state === "success"
              ? CheckCircleFillIcon
              : check.state === "failure" || check.state === "error"
                ? XCircleFillIcon
                : ClockIcon;
          return (
            <li key={check.context} className={check.state ?? "expected"}>
              <CheckIcon />
              <span>
                <strong>{check.context}</strong>
                <small>
                  {check.description ??
                    (check.state === null
                      ? "Expected — Waiting for status to be reported."
                      : check.state === "pending"
                        ? "In progress"
                        : check.state === "success"
                          ? "Successful"
                          : "Unsuccessful")}
                </small>
              </span>
              {check.run_id ? (
                <Link
                  href={repoHref(repo, {
                    view: "pulls",
                    pull: String(pull.number),
                    pull_tab: "checks",
                    check: String(check.run_id),
                  })}
                >
                  Details
                </Link>
              ) : check.target_url ? (
                <a href={check.target_url} target="_blank" rel="noreferrer">
                  Details
                </a>
              ) : null}
            </li>
          );
        })}
      </ul>
    </div>
  );
}

function PullTimeline({
  comments,
  reviews,
}: {
  comments: PullComment[];
  reviews: PullReview[];
}) {
  const events = [
    ...comments.map((comment) => ({
      kind: "comment" as const,
      value: comment,
    })),
    ...reviews.map((review) => ({ kind: "review" as const, value: review })),
  ].sort((left, right) => left.value.created_at - right.value.created_at);
  return (
    <div className="discussion-thread pull-timeline">
      {events.map((event) =>
        event.kind === "comment" ? (
          <article
            className="panel discussion-card"
            key={`comment-${event.value.number}`}
          >
            <header>
              <strong>{event.value.author}</strong>
              <span className="muted">
                commented {timestamp(event.value.created_at)}
              </span>
            </header>
            <DiscussionMarkdown>{event.value.body}</DiscussionMarkdown>
          </article>
        ) : (
          <ReviewEvent
            key={`review-${event.value.number}`}
            review={event.value}
          />
        ),
      )}
    </div>
  );
}

function ReviewEvent({ review }: { review: PullReview }) {
  const action =
    review.state === "approved"
      ? "approved these changes"
      : review.state === "changes_requested"
        ? "requested changes"
        : "left a review";
  const Icon =
    review.state === "approved"
      ? CheckCircleFillIcon
      : review.state === "changes_requested"
        ? XCircleFillIcon
        : CodeReviewIcon;
  return (
    <article className={`panel review-event ${review.state}`}>
      <header>
        <span className="review-icon" aria-hidden="true">
          <Icon />
        </span>
        <strong>{review.author}</strong>
        <span> {action}</span>
        {!review.current && <span className="review-outdated">Outdated</span>}
        <span className="muted">{timestamp(review.created_at)}</span>
      </header>
      {review.body && <DiscussionMarkdown>{review.body}</DiscussionMarkdown>}
      <footer className="muted">
        Reviewed commit <code>{short(review.commit_oid)}</code>
      </footer>
    </article>
  );
}

function ReviewForm({
  repo,
  pull,
  csrf,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  refresh: () => void;
}) {
  const [body, setBody] = useState("");
  const [state, setState] = useState<ReviewState>("commented");
  const mutation = useMutation(csrf);
  const submission = useSubmission();
  const required = state !== "approved";
  return (
    <form
      className="panel discussion-form review-form"
      onSubmit={async (event) => {
        event.preventDefault();
        const input = { body, state };
        const created = await mutation.run<PullReview>(
          endpoint(repo, `pulls/${pull.number}/reviews`),
          "POST",
          { ...input, request_id: submission(input) },
        );
        if (created) {
          refresh();
          navigate(
            repoHref(repo, { view: "pulls", pull: String(pull.number) }),
          );
        }
      }}
    >
      <h3>Submit your review</h3>
      <Editor
        id="pull-review"
        label="Review summary"
        value={body}
        onChange={setBody}
        disabled={mutation.pending}
        required={required}
      />
      <fieldset className="review-choices">
        <legend className="sr-only">Review decision</legend>
        <label>
          <input
            type="radio"
            name="review-state"
            value="commented"
            checked={state === "commented"}
            disabled={mutation.pending}
            onChange={() => setState("commented")}
          />
          <span>
            <strong>Comment</strong>
            <small>Leave feedback without an approval decision.</small>
          </span>
        </label>
        <label>
          <input
            type="radio"
            name="review-state"
            value="approved"
            checked={state === "approved"}
            disabled={mutation.pending || !pull.can_decide}
            onChange={() => setState("approved")}
          />
          <span>
            <strong>Approve</strong>
            <small>Accept the changes at the current head commit.</small>
          </span>
        </label>
        <label>
          <input
            type="radio"
            name="review-state"
            value="changes_requested"
            checked={state === "changes_requested"}
            disabled={mutation.pending || !pull.can_decide}
            onChange={() => setState("changes_requested")}
          />
          <span>
            <strong>Request changes</strong>
            <small>Block approval until the concerns are addressed.</small>
          </span>
        </label>
      </fieldset>
      {!pull.can_decide && (
        <p className="muted">
          Authors can comment, but cannot decide on their own changes.
        </p>
      )}
      <Failure message={mutation.error} />
      <div className="discussion-actions">
        <Button
          type="submit"
          variant="primary"
          disabled={mutation.pending || (required && !body.trim())}
        >
          {mutation.pending ? "Submitting…" : "Submit review"}
        </Button>
      </div>
    </form>
  );
}
