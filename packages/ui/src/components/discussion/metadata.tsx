import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { PersonAddIcon } from "@primer/octicons-react";
import {
  endpoint,
  useRequest,
  type Repository,
  type RepositoryAssignee,
  type RepositoryLabel,
} from "@/api";
import { ReviewRequests } from "@/collaboration/review-requests";
import { LinkedWork } from "@/collaboration/links";
import { MilestonePicker } from "@/collaboration/milestones/picker";
import { LabelBadges, LabelPicker } from "@/discussion-labels";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/ui";

interface AssigneeCatalog {
  items: RepositoryAssignee[];
  can_manage: boolean;
}

function initials(name: string) {
  return name
    .split(/\s+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((part) => part[0]?.toUpperCase())
    .join("");
}

export function AssigneeAvatars({
  assignees = [],
}: {
  assignees?: RepositoryAssignee[];
}) {
  if (!assignees.length) return null;
  return (
    <span
      className="assignee-avatars"
      role="img"
      aria-label={`Assigned to ${assignees.map((assignee) => assignee.name).join(", ")}`}
    >
      {assignees.map((assignee) => (
        <span
          className="assignee-avatar"
          key={assignee.subject}
          title={assignee.name}
          aria-hidden="true"
        >
          {initials(assignee.name)}
        </span>
      ))}
    </span>
  );
}

function AssigneePicker({
  repo,
  assignees: assigned = [],
  version,
  path,
  csrf,
  onSaved,
}: {
  repo: Repository;
  assignees?: RepositoryAssignee[];
  version: number;
  path: string;
  csrf: string;
  onSaved: () => void;
}) {
  const catalog = useRequest<AssigneeCatalog>(endpoint(repo, "assignees"));
  const mutation = useMutation(csrf);
  const details = useRef<HTMLDetailsElement>(null);
  const [selected, setSelected] = useState(() =>
    assigned.map((assignee) => assignee.subject),
  );
  useEffect(
    () => setSelected(assigned.map((assignee) => assignee.subject)),
    [assigned],
  );
  return (
    <div className="label-picker assignee-picker">
      <details ref={details}>
        <summary aria-label="Edit assignees">
          <PersonAddIcon />
        </summary>
        <div className="label-picker-menu">
          <header>
            <strong>Assign people</strong>
            <Button
              size="sm"
              variant="ghost"
              aria-label="Close assignee picker"
              onClick={() => details.current?.removeAttribute("open")}
            >
              ×
            </Button>
          </header>
          <Result state={catalog} timingLabel="Assignees">
            {(data) => (
              <>
                <fieldset disabled={mutation.pending}>
                  <legend className="sr-only">Repository members</legend>
                  {data.items.map((assignee) => (
                    <label key={assignee.subject}>
                      <input
                        type="checkbox"
                        checked={selected.includes(assignee.subject)}
                        onChange={(event) =>
                          setSelected((current) =>
                            event.target.checked
                              ? [...current, assignee.subject]
                              : current.filter(
                                  (subject) => subject !== assignee.subject,
                                ),
                          )
                        }
                      />
                      <span className="assignee-avatar" aria-hidden="true">
                        {initials(assignee.name)}
                      </span>
                      <span>{assignee.name}</span>
                    </label>
                  ))}
                </fieldset>
                {mutation.error && (
                  <p className="error" role="alert">
                    {mutation.error}
                  </p>
                )}
                <Button
                  size="sm"
                  variant="default"
                  disabled={mutation.pending || selected.length > 10}
                  onClick={async () => {
                    const saved = await mutation.run(
                      endpoint(repo, path),
                      "PATCH",
                      { version, assignees: selected },
                    );
                    if (saved) {
                      details.current?.removeAttribute("open");
                      onSaved();
                    }
                  }}
                >
                  {mutation.pending ? "Saving…" : "Apply assignees"}
                </Button>
              </>
            )}
          </Result>
        </div>
      </details>
    </div>
  );
}

export function DiscussionMetadata({
  repo,
  assignees = [],
  labels = [],
  canAssign = false,
  canLabel = false,
  version,
  path,
  csrf,
  onSaved,
}: {
  repo: Repository;
  assignees?: RepositoryAssignee[];
  labels?: RepositoryLabel[];
  canAssign?: boolean;
  canLabel?: boolean;
  version: number;
  path: string;
  csrf: string;
  onSaved: () => void;
}) {
  return (
    <aside className="discussion-metadata" aria-label="Metadata">
      {path.startsWith("pulls/") && (
        <ReviewRequests
          key={`reviewers:${version}`}
          repo={repo}
          pull={Number(path.split("/")[1])}
          csrf={csrf}
          onSaved={onSaved}
        />
      )}
      <section>
        <header>
          <strong>Assignees</strong>
          {canAssign && (
            <AssigneePicker
              repo={repo}
              assignees={assignees}
              version={version}
              path={path}
              csrf={csrf}
              onSaved={onSaved}
            />
          )}
        </header>
        {assignees.length ? (
          <ul className="metadata-assignees">
            {assignees.map((assignee) => (
              <li key={assignee.subject}>
                <span className="assignee-avatar" aria-hidden="true">
                  {initials(assignee.name)}
                </span>
                <span>{assignee.name}</span>
              </li>
            ))}
          </ul>
        ) : (
          <p className="muted">No one assigned</p>
        )}
      </section>
      <section>
        <header>
          <strong>Labels</strong>
          {canLabel && (
            <LabelPicker
              repo={repo}
              labels={labels}
              version={version}
              path={path}
              csrf={csrf}
              onSaved={onSaved}
              compact
            />
          )}
        </header>
        {labels.length ? (
          <LabelBadges labels={labels} />
        ) : (
          <p className="muted">None yet</p>
        )}
      </section>
      <LinkedWork
        key={`links:${version}`}
        repo={repo}
        path={path}
        csrf={csrf}
        onSaved={onSaved}
      />
      <MilestonePicker
        key={version}
        repo={repo}
        path={path}
        csrf={csrf}
        onSaved={onSaved}
      />
    </aside>
  );
}
