import { useState } from "react";
import { Button } from "@primer/react";
import {
  CheckCircleFillIcon,
  CodeReviewIcon,
  CommentIcon,
  GitMergeIcon,
  GitPullRequestClosedIcon,
  GitPullRequestIcon,
  XCircleFillIcon,
} from "@primer/octicons-react";
import {
  endpoint,
  navigate,
  repoHref,
  useRequest,
  type Refs,
  type Repository,
} from "./api";
import { ComparisonView } from "./content";
import { useMutation } from "./discussion-mutations";
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
  state: "open" | "closed";
  author: string;
  base_ref: string;
  head_ref: string;
  created_at: number;
  updated_at: number;
}

interface PullRequest extends PullSummary {
  body: string;
  version: number;
  can_edit: boolean;
  base_oid: string;
  head_oid: string;
  original_base_oid: string | null;
  original_head_oid: string | null;
  can_manage: boolean;
  can_decide: boolean;
  branches_available: boolean;
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
  return (
    <span className={`pull-state ${state}`} aria-live="polite">
      {state === "open" ? <GitPullRequestIcon /> : <GitPullRequestClosedIcon />}
      {state === "open" ? "Open" : "Closed"}
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
    return <NewPull repo={repo} refs={refs} csrf={csrf} theme={theme} />;
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
  const page = useRequest<Page<PullSummary>>(
    endpoint(repo, "pulls", { state, before }),
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
      <div className="issues-filters">
        <nav aria-label="Pull request state">
          {["open", "closed", "all"].map((value) => (
            <Link
              key={value}
              className={state === value ? "active" : ""}
              aria-current={state === value ? "page" : undefined}
              href={repoHref(repo, { view: "pulls", state: value })}
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
                  </li>
                ))}
              </ul>
            ) : (
              <div className="notice issue-empty">
                <GitPullRequestIcon size={32} />
                <h3>No matching pull requests</h3>
                <p>Compare two branches to start a review.</p>
              </div>
            )}
            <div className="discussion-pagination">
              {before && (
                <Link href={repoHref(repo, { view: "pulls", state })}>
                  Newest pull requests
                </Link>
              )}
              {data.next && (
                <Link
                  href={repoHref(repo, {
                    view: "pulls",
                    state,
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
  csrf,
  theme,
}: {
  repo: Repository;
  refs: Refs;
  csrf: string;
  theme: "light" | "dark";
}) {
  const branches = refs.refs.filter((ref) =>
    ref.name.startsWith("refs/heads/"),
  );
  const initialBase = refs.head?.name ?? branches[0]?.name ?? "";
  const [base, setBase] = useState(initialBase);
  const [head, setHead] = useState(
    branches.find((ref) => ref.name !== initialBase)?.name ?? initialBase,
  );
  const [title, setTitle] = useState("");
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
  const tab =
    url.searchParams.get("pull_tab") === "files" ? "files" : "conversation";
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
                <span>
                  <strong>{data.author}</strong> wants to merge{" "}
                  <code>{short(data.head_oid)}</code> from{" "}
                  <code>{branch(data.head_ref)}</code> into{" "}
                  <code>{branch(data.base_ref)}</code>
                </span>
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
            {!data.branches_available && (
              <div className="notice error">
                One of this pull request&apos;s branches no longer exists. The
                original commit IDs remain recorded, but the live comparison is
                unavailable.
              </div>
            )}
            {tab === "files" ? (
              data.branches_available ? (
                <>
                  <ComparisonView
                    repo={repo}
                    base={data.base_oid}
                    head={data.head_oid}
                    theme={theme}
                  />
                  {data.state === "open" && (
                    <ReviewForm repo={repo} pull={data} csrf={csrf} />
                  )}
                </>
              ) : null
            ) : (
              <PullConversation
                repo={repo}
                pull={data}
                csrf={csrf}
                refresh={pull.retry}
              />
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
        <GitMergeIcon />
        <div>
          <strong>Merge is not enabled yet</strong>
          <p>
            Review decisions and discussion are durable. Merge commits, checks,
            and protected-branch enforcement remain under development.
          </p>
        </div>
      </div>
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
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
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
        if (created)
          navigate(
            repoHref(repo, { view: "pulls", pull: String(pull.number) }),
          );
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
