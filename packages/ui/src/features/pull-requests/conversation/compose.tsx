import { useState } from "react";
import { endpoint, type Repository } from "@/lib/api";
import { Editor } from "@/components/discussion/editor";
import { Failure } from "@/components/discussion/failure";
import { useReturnFocus, useSubmission } from "@/components/discussion/hooks";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import type { PullComment, PullRequest } from "../model";

export function usePullComment(repo: Repository, pull: number, csrf: string) {
  const [body, setBody] = useState("");
  const [created, setCreated] = useState<PullComment>();
  const [posted, setPosted] = useState(false);
  const mutation = useMutation(csrf);
  const submission = useSubmission();
  return {
    body,
    created,
    posted,
    pending: mutation.pending,
    error: mutation.error,
    change(value: string) {
      setBody(value);
      setPosted(false);
    },
    async submit() {
      const input = { body };
      const saved = await mutation.run<PullComment>(
        endpoint(repo, `pulls/${pull}/comments`),
        "POST",
        { ...input, request_id: submission(input) },
      );
      if (saved) {
        setCreated(saved);
        setPosted(true);
        setBody((current) => (current === input.body ? "" : current));
        submission({ body: "" });
      }
    },
  };
}

export function PullCommentForm({
  repo,
  pull,
  csrf,
  refresh,
  comment,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  refresh: () => void;
  comment: ReturnType<typeof usePullComment>;
}) {
  const stateMutation = useMutation(csrf);
  const editor = useReturnFocus<HTMLTextAreaElement>(comment.pending);
  return (
    <form
      className="discussion-form pull-comment-form"
      onSubmit={async (event) => {
        event.preventDefault();
        await comment.submit();
      }}
    >
      <h3>Add a comment</h3>
      <Editor
        key={comment.created?.number ?? 0}
        ref={editor}
        id="pull-comment"
        label="Comment"
        value={comment.body}
        onChange={comment.change}
        disabled={comment.pending}
        required
      />
      <Failure message={comment.error} />
      <div role="status">{comment.posted ? "Comment posted." : ""}</div>
      <div className="discussion-actions">
        {pull.can_manage && (
          <Button
            variant={pull.state === "open" ? "destructive" : "outline"}
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
        <Button
          type="submit"
          disabled={comment.pending || !comment.body.trim()}
        >
          {comment.pending ? "Commenting…" : "Comment"}
        </Button>
      </div>
      <Failure message={stateMutation.error} />
    </form>
  );
}
