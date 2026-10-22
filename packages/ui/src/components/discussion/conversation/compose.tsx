import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { endpoint, type Repository } from "@/lib/api";
import { cn } from "@/lib/utils";
import { Editor } from "../editor";
import { Failure } from "../failure";
import { useReturnFocus, useSubmission } from "../hooks";
import { useMutation } from "../mutations";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import type { Comment } from "./model";

export function useCommentSubmission(
  repo: Repository,
  target: string,
  csrf: string,
) {
  const [body, setBody] = useState("");
  const [created, setCreated] = useState<Comment>();
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
      const saved = await mutation.run<Comment>(
        endpoint(repo, `${target}/comments`),
        "POST",
        { ...input, request_id: submission(input) },
      );
      if (saved) {
        setCreated(saved);
        setPosted(true);
        setBody((current) => (current === input.body ? "" : current));
        submission({ body: "" });
      }
      return saved;
    },
  };
}

export function CommentComposer({
  comment,
  id,
  actions,
  busy = false,
  className,
  href,
  onCreated,
}: {
  comment: ReturnType<typeof useCommentSubmission>;
  id: string;
  actions?: ReactNode;
  busy?: boolean;
  className?: string;
  href: string;
  onCreated: (comment: Comment) => void;
}) {
  const form = useRef<HTMLFormElement>(null);
  const completed = useRef(onCreated);
  useLayoutEffect(() => {
    completed.current = onCreated;
  });
  const editor = useReturnFocus<HTMLTextAreaElement>(comment.pending);
  return (
    <form
      ref={form}
      className={cn("discussion-compose", className)}
      onSubmit={async (event) => {
        event.preventDefault();
        const saved = await comment.submit();
        // A completed write stays in the controller across tabs. Only continue
        // the visible window; a delayed response must not change navigation.
        if (saved && form.current?.getClientRects().length)
          completed.current(saved);
      }}
    >
      <h3>Add a comment</h3>
      <Editor
        key={comment.created?.number ?? 0}
        ref={editor}
        id={id}
        label="Comment"
        value={comment.body}
        onChange={comment.change}
        disabled={comment.pending}
        required
      />
      <Failure message={comment.error} />
      <div role="status">{comment.posted ? "Comment posted." : ""}</div>
      {comment.posted && comment.created && (
        <Link href={`${href}#issuecomment-${comment.created.number}`}>
          View comment
        </Link>
      )}
      <div className="discussion-actions">
        {actions}
        <Button
          type="submit"
          disabled={busy || comment.pending || !comment.body.trim()}
        >
          {comment.pending ? "Posting…" : "Comment"}
        </Button>
      </div>
    </form>
  );
}
