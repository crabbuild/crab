import { useState } from "react";
import { ActionList, ActionMenu, Button, TextInput } from "@primer/react";
import {
  CheckCircleFillIcon,
  ChecklistIcon,
  ClockIcon,
  CodeReviewIcon,
  CommentIcon,
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
  useRequest,
  type Refs,
  type Repository,
  type RepositoryAssignee,
  type RepositoryLabel,
} from "./api";
import { ComparisonView } from "./content";
import { CheckRuns } from "./check-runs";
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

interface Page<T> {
  items: T[];
  next: number | null;
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
}: {
  repo: Repository;
  refs: Refs;
  url: URL;
  csrf: string;
  theme: "light" | "dark";
}) {
  const pull = url.searchParams.get("pull");
  if (pull === "new")
    return (
      <NewPull repo={repo} refs={refs} url={url} csrf={csrf} theme={theme} />
    );
  if (pull) {
    const number = Number(pull);
    if (!Number.isSafeInteger(number) || number <= 0)
      return (
        <div className="notice error">
          <h1>Pull request not found</h1>
          <Link href={repoHref(repo, { view: "pulls" })}>
            Back to pull requests
          </Link>
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
        <h2>Pull requests</h2>
        <Button
          variant="primary"
          onClick={() =>
            navigate(repoHref(repo, { view: "pulls", pull: "new" }))
          }
        >
          New pull request
        </Button>
      </div>
      <p className="muted">Review and discuss changes between branches.</p>
      <DiscussionSearch
        label="Search pull requests"
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
                ? "All pull requests"
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
                  {query
                    ? `No pull requests match “${query}”`
                    : "No matching pull requests"}
                </h3>
                <p>
                  {query
                    ? data.next
                      ? "Try another search or continue to older pull requests."
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
                  Newest pull requests
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
                  Older pull requests →
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
}: {
  repo: Repository;
  refs: Refs;
  url: URL;
  csrf: string;
  theme: "light" | "dark";
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
      <Link href={repoHref(repo, { view: "pulls" })}>
        ← Back to pull requests
      </Link>
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
}: {
  repo: Repository;
  number: number;
  url: URL;
  csrf: string;
  theme: "light" | "dark";
}) {
  const path = endpoint(repo, `pulls/${number}`);
  const pull = useRequest<PullRequest>(path);
  const requestedTab = url.searchParams.get("pull_tab");
  const tab =
    requestedTab === "files" || requestedTab === "checks"
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
                    <strong>{data.merge.author}</strong> fast-forwarded{" "}
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
            ) : tab === "files" ? (
              data.branches_available ? (
                <>
                  <ComparisonView
                    repo={repo}
                    base={data.base_oid}
                    head={data.head_oid}
                    theme={theme}
                  />
                  {data.state === "open" && (
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
                  canAssign={data.can_assign}
                  canLabel={data.can_label}
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
      <div className="notice pull-merge-note">
        <MergePanel repo={repo} pull={pull} csrf={csrf} refresh={refresh} />
      </div>
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
