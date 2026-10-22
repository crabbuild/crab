import { Author } from "@/components/discussion/author";
import { Reactions } from "@/components/discussion/reactions";
import { repoHref } from "@/lib/repository-route";
import { useCallback, useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";

import { CircleSlashIcon } from "lucide-react";
import { IssueClosedIcon, IssueOpenedIcon } from "@primer/octicons-react";
import { IssueStateControls } from "./state";
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

function IssueBadge({ issue }: { issue: Issue }) {
  const { state, state_reason: reason } = issue;
  const notPlanned = state === "closed" && reason === "not_planned";
  return (
    <span
      className={`issue-state ${notPlanned ? "not-planned" : state}`}
      aria-live="polite"
    >
      {state === "open" ? (
        <IssueOpenedIcon />
      ) : notPlanned ? (
        <CircleSlashIcon className="size-4" />
      ) : (
        <IssueClosedIcon />
      )}
      {state === "open"
        ? "Open"
        : notPlanned
          ? "Closed as not planned"
          : "Closed"}
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
  // A delayed state/edit response must not replace a newer metadata refresh.
  const accept = useCallback((incoming: Issue) => {
    setIssue((old) => (old && old.version > incoming.version ? old : incoming));
  }, []);
  useEffect(() => {
    if (resource.data) accept(resource.data);
  }, [resource.data, accept]);
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
        <IssueBadge issue={current} />
        <span className="muted">
          <Author author={current.author} /> opened this issue{" "}
          {timestamp(current.created_at)}
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
              onReviewed={accept}
              onSaved={(issue) => {
                accept(issue);
                setEditing(false);
              }}
            />
          ) : (
            <article className="discussion-card panel">
              <header>
                <strong>
                  <Author author={current.author} />
                </strong>
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
          <Comments
            repo={repo}
            issue={number}
            version={`${current.version}:${metadataEpoch}`}
            csrf={csrf}
            actions={(busy) =>
              current.can_close &&
              !repo.archived && (
                <IssueStateControls
                  issue={current}
                  repo={repo}
                  csrf={csrf}
                  disabled={busy || editing}
                  onSaved={accept}
                  onRefresh={resource.retry}
                />
              )
            }
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
