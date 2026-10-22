import { useState } from "react";
import { Button } from "@/components/ui/button";
import { endpoint, navigate, useRequest, type Repository } from "@/lib/api";
import { useMutation } from "@/components/discussion/mutations";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { Editor } from "@/components/discussion/editor";
import { useSubmission } from "@/components/discussion/hooks";
import { repoHref } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import type { Milestone, MilestoneDetail } from "./model";

export function MilestoneForm({
  repo,
  milestone,
  csrf,
  onCancel,
  onSaved,
}: {
  repo: Repository;
  milestone?: Milestone;
  csrf: string;
  onCancel?: () => void;
  onSaved?: (value: Milestone) => void;
}) {
  const [title, setTitle] = useState(milestone?.title ?? "");
  const [description, setDescription] = useState(milestone?.description ?? "");
  const [due, setDue] = useState(milestone?.due_on ?? "");
  const [version, setVersion] = useState(milestone?.version ?? 0);
  const mutation = useMutation(csrf, repo);
  const submission = useSubmission();
  const target = endpoint(
    repo,
    milestone ? `milestones/${milestone.number}` : "milestones",
  );
  const saved = useRequest<MilestoneDetail>(
    mutation.conflict && milestone ? target : null,
  );
  return (
    <form
      className="milestone-form panel"
      onSubmit={async (event) => {
        event.preventDefault();
        const fields = { title, description, due_on: due || null };
        const input = milestone
          ? { ...fields, version }
          : { ...fields, request_id: submission(fields) };
        const value = await mutation.run<Milestone>(
          target,
          milestone ? "PATCH" : "POST",
          input,
        );
        if (!value) return;
        if (onSaved) onSaved(value);
        else
          navigate(
            repoHref(repo, {
              view: "milestones",
              milestone: String(value.number),
            }),
          );
      }}
    >
      <h2>{milestone ? "Edit milestone" : "New milestone"}</h2>
      <p className="muted">
        Group related issues and pull requests, then follow their progress.
      </p>
      <fieldset disabled={mutation.pending}>
        <label htmlFor="milestone-title">Title</label>
        <input
          id="milestone-title"
          value={title}
          onChange={(event) => setTitle(event.target.value)}
          required
          maxLength={256}
          autoFocus
        />
        <label htmlFor="milestone-due">
          Due date <span className="muted">(optional)</span>
        </label>
        <input
          id="milestone-due"
          type="date"
          min="0001-01-01"
          max="9999-12-31"
          value={due}
          onChange={(event) => setDue(event.target.value)}
        />
        <Editor
          id="milestone-description"
          label="Description (optional)"
          value={description}
          onChange={setDescription}
          disabled={mutation.pending}
        />
      </fieldset>
      {mutation.error && (
        <p className="notice error" role="alert">
          {mutation.error}
        </p>
      )}
      {mutation.conflict && (
        <Result state={saved} timingLabel="Saved milestone">
          {({ milestone: latest }) => (
            <section className="milestone-conflict notice">
              <strong>Review the saved milestone</strong>
              <h3>{latest.title}</h3>
              <p>
                {latest.state === "open" ? "Open" : "Closed"} ·{" "}
                {latest.due_on ? `Due ${latest.due_on}` : "No due date"}
              </p>
              <DiscussionMarkdown>{latest.description}</DiscussionMarkdown>
              <p className="muted">
                Your draft stays in this form. Review the saved content before
                saving again.
              </p>
              <div className="discussion-actions">
                <Button
                  variant="outline"
                  type="button"
                  onClick={() => {
                    setVersion(latest.version);
                    mutation.reset();
                  }}
                >
                  Continue with my draft
                </Button>
                <Button
                  variant="outline"
                  type="button"
                  onClick={() => {
                    setTitle(latest.title);
                    setDescription(latest.description);
                    setDue(latest.due_on ?? "");
                    setVersion(latest.version);
                    mutation.reset();
                  }}
                >
                  Use saved version
                </Button>
              </div>
            </section>
          )}
        </Result>
      )}
      <div className="discussion-actions">
        <Button
          type="submit"
          variant="default"
          disabled={mutation.pending || mutation.conflict || !title.trim()}
        >
          {mutation.pending
            ? "Saving…"
            : milestone
              ? "Save milestone"
              : "Create milestone"}
        </Button>
        {onCancel ? (
          <Button variant="outline" type="button" onClick={onCancel}>
            Cancel
          </Button>
        ) : (
          <Link
            className="button-link"
            href={repoHref(repo, { view: "milestones" })}
          >
            Cancel
          </Link>
        )}
      </div>
    </form>
  );
}
