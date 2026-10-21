import { Reactions } from "@/components/discussion/reactions";
import { repoHref } from "@/lib/repository-route";
import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";

import { IssueClosedIcon, IssueOpenedIcon } from "@primer/octicons-react";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { ConflictReview, useMutation } from "@/components/discussion/mutations";

import { DiscussionMetadata } from "@/components/discussion/metadata";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { Editor } from "@/components/discussion/editor";
import { Failure } from "@/components/discussion/failure";
import { useReturnFocus } from "@/components/discussion/hooks";

import { Issue } from "./model";
import { timestamp } from "@/components/discussion/time";
import { Comments } from "./comments";

function IssueBadge({ state }: { state: Issue["state"] }) {
  return (
    <span className={`issue-state ${state}`} aria-live="polite">
      {state === "open" ? <IssueOpenedIcon /> : <IssueClosedIcon />}
      {state === "open" ? "Open" : "Closed"}
    </span>
  );
}

export function IssueDetail({
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
  const [metadataEpoch, setMetadataEpoch] = useState(0);
  const mutation = useMutation(csrf);
  const editTrigger = useReturnFocus<HTMLButtonElement>(editing);
  const stateTrigger = useReturnFocus<HTMLButtonElement>(mutation.pending);
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
            variant="outline"
            size="sm"
            onClick={resource.retry}
            aria-busy={resource.loading}
          >
            Refresh
          </Button>
          {current.can_edit && !repo.archived && (
            <Button
              variant="outline"
              ref={editTrigger}
              size="sm"
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
      <div className="discussion-detail-layout">
        <div className="discussion-main">
          {editing ? (
            <EditIssue
              key={current.number}
              issue={current}
              repo={repo}
              csrf={csrf}
              onCancel={() => setEditing(false)}
              onReviewed={(latest) =>
                setIssue((old) =>
                  old && old.version > latest.version ? old : latest,
                )
              }
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
              <Reactions
                repo={repo}
                target={`issues/${number}`}
                summary={current.reactions}
                csrf={csrf}
              />
            </article>
          )}
          {current.can_edit && !repo.archived && (
            <div className="issue-state-controls">
              <Button
                variant="outline"
                ref={stateTrigger}
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
          <Comments
            repo={repo}
            issue={number}
            version={`${current.version}:${metadataEpoch}`}
            csrf={csrf}
          />
        </div>
        <DiscussionMetadata
          repo={repo}
          assignees={current.assignees}
          labels={current.labels}
          canAssign={current.can_assign && !repo.archived}
          canLabel={current.can_label && !repo.archived}
          version={current.version}
          path={`issues/${number}`}
          csrf={csrf}
          onSaved={() => {
            resource.retry();
            setMetadataEpoch((value) => value + 1);
          }}
        />
      </div>
    </section>
  );
}

function EditIssue({
  issue,
  repo,
  csrf,
  onCancel,
  onSaved,
  onReviewed,
}: {
  issue: Issue;
  repo: Repository;
  csrf: string;
  onCancel: () => void;
  onSaved: (issue: Issue) => void;
  onReviewed: (issue: Issue) => void;
}) {
  const [title, setTitle] = useState(issue.title);
  const [body, setBody] = useState(issue.body);
  const mutation = useMutation(csrf);
  // The edit retains the version at which the draft began, even if the parent refreshes.
  const version = useRef(issue.version);
  const save = useReturnFocus<HTMLButtonElement>(mutation.conflict);
  return (
    <form
      className="discussion-compose panel"
      onSubmit={async (event) => {
        event.preventDefault();
        if (mutation.conflict) return;
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
        autoFocus
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
      {mutation.conflict ? (
        <ConflictReview<Issue>
          url={endpoint(repo, `issues/${issue.number}`)}
          onResolve={(latest, choice) => {
            // Only explicit review advances the draft's version; background refresh cannot.
            version.current = latest.version;
            if (choice === "saved") {
              setTitle(latest.title);
              setBody(latest.body);
            }
            onReviewed(latest);
            mutation.reset();
          }}
        />
      ) : (
        <Failure message={mutation.error} />
      )}
      <div className="discussion-actions">
        <Button
          ref={save}
          type="submit"
          variant="default"
          disabled={mutation.pending || mutation.conflict || !title.trim()}
        >
          Save changes
        </Button>
        <Button
          variant="outline"
          type="button"
          disabled={mutation.pending}
          onClick={onCancel}
        >
          Cancel edit
        </Button>
      </div>
    </form>
  );
}
