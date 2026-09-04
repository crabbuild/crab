import { useEffect, useRef, useState } from "react";
import { Button, Spinner } from "@primer/react";
import { IssueClosedIcon, IssueOpenedIcon } from "@primer/octicons-react";
import {
  endpoint,
  navigate,
  repoHref,
  useRequest,
  type Repository,
} from "./api";
import { Link, Result } from "./ui";
import { DiscussionMarkdown, Editor, Failure } from "./discussion";

interface Comment {
  number: number;
  author: string;
  body: string;
  version: number;
  created_at: number;
  updated_at: number;
  can_edit: boolean;
}
interface Issue extends Comment {
  title: string;
  state: "open" | "closed";
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
function useMutation(csrf: string) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string>();
  const active = useRef(true);
  const busy = useRef(false);
  useEffect(() => {
    active.current = true;
    return () => {
      active.current = false;
    };
  }, []);
  async function run<T>(
    url: string,
    method: "POST" | "PATCH",
    input: object,
  ): Promise<T | undefined> {
    if (busy.current) return;
    busy.current = true;
    setPending(true);
    setError(undefined);
    try {
      const response = await fetch(url, {
        method,
        headers: { "Content-Type": "application/json", "X-CSRF-Token": csrf },
        body: JSON.stringify(input),
        signal: AbortSignal.timeout(35_000),
      });
      if (response.status === 401)
        window.dispatchEvent(new Event("crab-session-expired"));
      const body: unknown = await response.json().catch(() => null);
      if (!response.ok) {
        const failure = body as { error?: { message?: string } } | null;
        throw new Error(
          failure?.error?.message ?? `Request failed (${response.status})`,
        );
      }
      if (active.current) return body as T;
    } catch (error) {
      if (active.current)
        setError(
          error instanceof Error &&
            error.name !== "TimeoutError" &&
            error.name !== "TypeError"
            ? error.message
            : "The response was lost. Retry this submission to recover a possible completed write.",
        );
    } finally {
      busy.current = false;
      if (active.current) setPending(false);
    }
  }
  return { run, pending, error };
}

// Retain the key for an unchanged submission so retrying an ambiguous response cannot duplicate it.
function useSubmission() {
  const current = useRef({ body: "", id: crypto.randomUUID() });
  return (input: object) => {
    const body = JSON.stringify(input);
    if (body !== current.current.body)
      current.current = { body, id: crypto.randomUUID() };
    return current.current.id;
  };
}

function IssueBadge({ state }: { state: Issue["state"] }) {
  return (
    <span className={`issue-state ${state}`}>
      {state === "open" ? <IssueOpenedIcon /> : <IssueClosedIcon />}
      {state === "open" ? "Open" : "Closed"}
    </span>
  );
}

export function Issues({
  repo,
  url,
  csrf,
}: {
  repo: Repository;
  url: URL;
  csrf: string;
}) {
  const issue = url.searchParams.get("issue");
  if (issue === "new") return <NewIssue key="new" repo={repo} csrf={csrf} />;
  if (issue) {
    const number = Number(issue);
    if (!Number.isSafeInteger(number) || number <= 0)
      return (
        <div className="notice error">
          <h1>Issue not found</h1>
          <Link href={repoHref(repo, { view: "issues" })}>Back to issues</Link>
        </div>
      );
    return <IssueDetail key={number} repo={repo} number={number} csrf={csrf} />;
  }
  return <IssueList repo={repo} url={url} />;
}

function IssueList({ repo, url }: { repo: Repository; url: URL }) {
  const state = url.searchParams.get("state") ?? "open";
  const before = url.searchParams.get("before") ?? undefined;
  const page = useRequest<Page<Issue>>(
    endpoint(repo, "issues", { state, before }),
  );
  return (
    <section className="issues-page">
      <div className="section-heading">
        <h2>Issues</h2>
        <Button
          variant="primary"
          onClick={() =>
            navigate(repoHref(repo, { view: "issues", issue: "new" }))
          }
        >
          New issue
        </Button>
      </div>
      <p className="muted">
        Track work, report problems, and discuss changes with your team.
      </p>
      <div className="issues-filters">
        <nav aria-label="Issue state">
          {["open", "closed", "all"].map((value) => (
            <Link
              key={value}
              className={state === value ? "active" : ""}
              aria-current={state === value ? "page" : undefined}
              href={repoHref(repo, { view: "issues", state: value })}
            >
              {value === "all"
                ? "All issues"
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
                {data.items.map((issue) => (
                  <li key={issue.number}>
                    <span
                      className={`issue-status-icon ${issue.state}`}
                      aria-label={issue.state}
                    >
                      {issue.state === "open" ? (
                        <IssueOpenedIcon />
                      ) : (
                        <IssueClosedIcon />
                      )}
                    </span>
                    <div>
                      <Link
                        className="issue-link"
                        href={repoHref(repo, {
                          view: "issues",
                          issue: String(issue.number),
                        })}
                      >
                        {issue.title}
                      </Link>
                      <p className="muted">
                        #{issue.number} opened {timestamp(issue.created_at)} by{" "}
                        {issue.author}
                      </p>
                    </div>
                  </li>
                ))}
              </ul>
            ) : (
              <div className="notice issue-empty">
                <IssueOpenedIcon size={32} />
                <h3>
                  {data.next
                    ? "No matching issues in this range"
                    : "No matching issues"}
                </h3>
                <p>
                  {data.next
                    ? "Continue to older issues or choose another filter."
                    : "Start a discussion by opening a new issue."}
                </p>
              </div>
            )}
            <div className="discussion-pagination">
              {before && (
                <Link href={repoHref(repo, { view: "issues", state })}>
                  Newest issues
                </Link>
              )}
              {data.next && (
                <Link
                  href={repoHref(repo, {
                    view: "issues",
                    state,
                    before: String(data.next),
                  })}
                >
                  Older issues →
                </Link>
              )}
            </div>
          </>
        )}
      </Result>
    </section>
  );
}

function NewIssue({ repo, csrf }: { repo: Repository; csrf: string }) {
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const mutation = useMutation(csrf);
  const submission = useSubmission();
  return (
    <section className="discussion-compose issues-page">
      <Link href={repoHref(repo, { view: "issues" })}>← Back to issues</Link>
      <h2>New issue</h2>
      <form
        onSubmit={async (event) => {
          event.preventDefault();
          const input = { title, body };
          const issue = await mutation.run<Issue>(
            endpoint(repo, "issues"),
            "POST",
            { ...input, request_id: submission(input) },
          );
          if (issue)
            navigate(
              repoHref(repo, { view: "issues", issue: String(issue.number) }),
            );
        }}
      >
        <label htmlFor="issue-title">Title</label>
        <input
          id="issue-title"
          autoFocus
          required
          maxLength={256}
          value={title}
          onChange={(event) => setTitle(event.target.value)}
          disabled={mutation.pending}
          placeholder="A short, descriptive title"
        />
        <Editor
          id="issue-description"
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
            {mutation.pending ? "Creating issue…" : "Create issue"}
          </Button>
          <Link href={repoHref(repo, { view: "issues" })}>Cancel</Link>
        </div>
      </form>
    </section>
  );
}

function IssueDetail({
  repo,
  number,
  csrf,
}: {
  repo: Repository;
  number: number;
  csrf: string;
}) {
  const resource = useRequest<Issue>(endpoint(repo, `issues/${number}`));
  const [issue, setIssue] = useState<Issue>();
  const [editing, setEditing] = useState(false);
  const mutation = useMutation(csrf);
  useEffect(() => {
    const incoming = resource.data;
    if (incoming)
      setIssue((old) =>
        old && old.version > incoming.version ? old : incoming,
      );
  }, [resource.data]);
  const current = issue ?? resource.data;
  if (!current) return <Result state={resource}>{() => null}</Result>;
  return (
    <section className="issues-page issue-detail">
      <Link href={repoHref(repo, { view: "issues" })}>← Back to issues</Link>
      <div className="issue-title-row">
        <h2>
          {current.title} <span className="muted">#{current.number}</span>
        </h2>
        <div className="discussion-actions">
          <Button
            size="small"
            onClick={resource.retry}
            disabled={resource.loading}
          >
            Refresh
          </Button>
          {current.can_edit && (
            <Button
              size="small"
              disabled={mutation.pending || editing}
              onClick={() => setEditing(true)}
            >
              Edit issue
            </Button>
          )}
        </div>
      </div>
      <p className="issue-meta">
        <IssueBadge state={current.state} />
        <span className="muted">
          {current.author} opened this issue {timestamp(current.created_at)}
        </span>
      </p>
      {resource.error && (
        <p className="notice error" role="alert">
          {resource.error}
        </p>
      )}
      {editing ? (
        <EditIssue
          key={current.number}
          issue={current}
          repo={repo}
          csrf={csrf}
          onCancel={() => setEditing(false)}
          onSaved={(issue) => {
            setIssue(issue);
            setEditing(false);
          }}
        />
      ) : (
        <article className="discussion-card panel">
          <header>
            <strong>{current.author}</strong>
            <time dateTime={new Date(current.created_at).toISOString()}>
              {timestamp(current.created_at)}
            </time>
            {current.version > 1 && <span className="muted">edited</span>}
          </header>
          <DiscussionMarkdown>{current.body}</DiscussionMarkdown>
        </article>
      )}
      {current.can_edit && (
        <div className="issue-state-controls">
          <Button
            disabled={mutation.pending || editing}
            onClick={async () => {
              const updated = await mutation.run<Issue>(
                endpoint(repo, `issues/${number}`),
                "PATCH",
                {
                  version: current.version,
                  state: current.state === "open" ? "closed" : "open",
                },
              );
              if (updated) setIssue(updated);
            }}
          >
            {current.state === "open" ? "Close issue" : "Reopen issue"}
          </Button>
          {mutation.error && (
            <p className="notice error" role="alert">
              {mutation.error}
            </p>
          )}
        </div>
      )}
      <Comments repo={repo} issue={number} csrf={csrf} />
    </section>
  );
}

function EditIssue({
  issue,
  repo,
  csrf,
  onCancel,
  onSaved,
}: {
  issue: Issue;
  repo: Repository;
  csrf: string;
  onCancel: () => void;
  onSaved: (issue: Issue) => void;
}) {
  const [title, setTitle] = useState(issue.title);
  const [body, setBody] = useState(issue.body);
  const mutation = useMutation(csrf);
  // The edit retains the version at which the draft began, even if the parent refreshes.
  const version = useRef(issue.version);
  return (
    <form
      className="discussion-compose panel"
      onSubmit={async (event) => {
        event.preventDefault();
        const updated = await mutation.run<Issue>(
          endpoint(repo, `issues/${issue.number}`),
          "PATCH",
          { version: version.current, title, body },
        );
        if (updated) onSaved(updated);
      }}
    >
      <label htmlFor="edit-issue-title">Title</label>
      <input
        id="edit-issue-title"
        value={title}
        onChange={(event) => setTitle(event.target.value)}
        disabled={mutation.pending}
        maxLength={256}
        required
      />
      <Editor
        id="edit-issue-description"
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
          Save changes
        </Button>
        <Button type="button" disabled={mutation.pending} onClick={onCancel}>
          Cancel edit
        </Button>
      </div>
    </form>
  );
}

function Comments({
  repo,
  issue,
  csrf,
}: {
  repo: Repository;
  issue: number;
  csrf: string;
}) {
  const [before, setBefore] = useState<string>();
  const page = useRequest<Page<Comment>>(
    endpoint(repo, `issues/${issue}/comments`, { before }),
  );
  const [items, setItems] = useState<Record<number, Comment>>({});
  useEffect(() => {
    const incoming = page.data;
    if (incoming)
      setItems((old) => {
        const merged = { ...old };
        for (const item of incoming.items) {
          // An in-flight page must not replace a newer successful edit.
          if (
            !merged[item.number] ||
            merged[item.number].version <= item.version
          )
            merged[item.number] = item;
        }
        return merged;
      });
  }, [page.data]);
  function upsert(comment: Comment) {
    setItems((old) => ({ ...old, [comment.number]: comment }));
  }
  return (
    <section className="issue-comments" aria-label="Discussion">
      <div className="section-heading">
        <h3>Discussion</h3>
        <Button
          size="small"
          disabled={page.loading}
          onClick={() => {
            setBefore(undefined);
            page.retry();
          }}
        >
          Refresh discussion
        </Button>
      </div>
      {page.data?.next && (
        <Button
          className="load-comments"
          onClick={() => setBefore(String(page.data?.next))}
          disabled={page.loading}
        >
          Load earlier comments
        </Button>
      )}
      {page.loading && (
        <p role="status">
          <Spinner size="small" /> Loading discussion…
        </p>
      )}
      {page.error && (
        <div className="notice error" role="alert">
          <p>{page.error}</p>
          <Button onClick={page.retry}>Try again</Button>
        </div>
      )}
      {!page.loading && !page.error && !Object.keys(items).length && (
        <p className="muted">
          {page.data?.next
            ? "No comments in this range. Load earlier comments."
            : "No comments yet. Start the conversation."}
        </p>
      )}
      {Object.values(items)
        .sort((a, b) => a.number - b.number)
        .map((comment) => (
          <CommentCard
            key={comment.number}
            comment={comment}
            repo={repo}
            issue={issue}
            csrf={csrf}
            onSaved={upsert}
          />
        ))}
      <NewComment repo={repo} issue={issue} csrf={csrf} onCreated={upsert} />
    </section>
  );
}

function NewComment({
  repo,
  issue,
  csrf,
  onCreated,
}: {
  repo: Repository;
  issue: number;
  csrf: string;
  onCreated: (comment: Comment) => void;
}) {
  const [body, setBody] = useState("");
  const mutation = useMutation(csrf);
  const submission = useSubmission();
  return (
    <form
      className="discussion-compose new-comment"
      onSubmit={async (event) => {
        event.preventDefault();
        const input = { body };
        const comment = await mutation.run<Comment>(
          endpoint(repo, `issues/${issue}/comments`),
          "POST",
          { ...input, request_id: submission(input) },
        );
        if (comment) {
          onCreated(comment);
          setBody("");
          submission({ body: "" });
        }
      }}
    >
      <h3>Add a comment</h3>
      <Editor
        id="new-comment"
        label="Comment"
        value={body}
        onChange={setBody}
        disabled={mutation.pending}
        required
      />
      <Failure message={mutation.error} />
      <div className="discussion-actions">
        <Button
          type="submit"
          variant="primary"
          disabled={mutation.pending || !body.trim()}
        >
          {mutation.pending ? "Posting…" : "Comment"}
        </Button>
      </div>
    </form>
  );
}

function CommentCard({
  comment,
  repo,
  issue,
  csrf,
  onSaved,
}: {
  comment: Comment;
  repo: Repository;
  issue: number;
  csrf: string;
  onSaved: (comment: Comment) => void;
}) {
  const [draft, setDraft] = useState<{ body: string; version: number }>();
  const mutation = useMutation(csrf);
  return (
    <article id={`comment-${comment.number}`} className="discussion-card panel">
      <header>
        <strong>{comment.author}</strong>
        <time dateTime={new Date(comment.created_at).toISOString()}>
          {timestamp(comment.created_at)}
        </time>
        {comment.version > 1 && <span className="muted">edited</span>}
        {comment.can_edit && !draft && (
          <Button
            size="small"
            onClick={() =>
              setDraft({ body: comment.body, version: comment.version })
            }
          >
            Edit comment
          </Button>
        )}
      </header>
      {draft ? (
        <form
          className="comment-edit"
          onSubmit={async (event) => {
            event.preventDefault();
            const updated = await mutation.run<Comment>(
              endpoint(repo, `issues/${issue}/comments/${comment.number}`),
              "PATCH",
              draft,
            );
            if (updated) {
              onSaved(updated);
              setDraft(undefined);
            }
          }}
        >
          <Editor
            id={`edit-comment-${comment.number}`}
            label="Edit comment"
            value={draft.body}
            onChange={(body) => setDraft({ ...draft, body })}
            disabled={mutation.pending}
            required
          />
          <Failure message={mutation.error} />
          <div className="discussion-actions">
            <Button
              type="submit"
              variant="primary"
              disabled={mutation.pending || !draft.body.trim()}
            >
              Save comment
            </Button>
            <Button
              type="button"
              onClick={() => setDraft(undefined)}
              disabled={mutation.pending}
            >
              Cancel edit
            </Button>
          </div>
        </form>
      ) : (
        <DiscussionMarkdown>{comment.body}</DiscussionMarkdown>
      )}
    </article>
  );
}
