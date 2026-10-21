import { endpoint, navigate, type Repository } from "@/lib/api";
import { Editor } from "@/components/discussion/editor";
import { Failure } from "@/components/discussion/failure";
import { useSubmission } from "@/components/discussion/hooks";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import { repoHref } from "@/lib/repository-route";
import { useState } from "react";

import type { PullRequest, PullReview, ReviewState } from "../model";

export function ReviewForm({
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
          variant="default"
          disabled={mutation.pending || (required && !body.trim())}
        >
          {mutation.pending ? "Submitting…" : "Submit review"}
        </Button>
      </div>
    </form>
  );
}
