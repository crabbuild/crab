import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { endpoint, type Repository } from "@/lib/api";
import { cn } from "@/lib/utils";
import { Editor } from "../editor";
import { Failure } from "../failure";
import { useReturnFocus, useSubmission } from "../hooks";
import { useMutation } from "../mutations";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import type { Comment } from "./model";

export interface StateChange {
  version: number;
  state: "open" | "closed";
  reason?: "completed" | "not_planned";
}
type CommentInput = { request_id: string; body: string };
type StateInput = StateChange & { request_id: string; body?: string };
type Intent =
  | { kind: "comment"; input: CommentInput }
  | { kind: "state"; input: StateInput };
type Saved = { comment: Comment | null; stateChanged: boolean };
type ChangeState = (
  input: StateChange,
  comment?: "include" | "omit",
) => Promise<void>;

export function useCommentSubmission(
  repo: Repository,
  target: string,
  csrf: string,
) {
  const [body, setBody] = useState("");
  const [created, setCreated] = useState<Comment>();
  const [posted, setPosted] = useState(false);
  const [validation, setValidation] = useState<string>();
  const [intent, setIntent] = useState<Intent>();
  const busy = useRef(false);
  const mutation = useMutation(csrf);
  const submission = useSubmission();
  // State replies can fail permission checks after the transaction committed.
  // Only their explicit rejection marker proves that no state/comment was saved.
  const uncertain =
    intent?.kind === "state"
      ? !mutation.rejected
      : mutation.status === undefined ||
        mutation.status >= 500 ||
        [403, 404, 408].includes(mutation.status);
  // A definite rejection permits a revised intent. Ambiguous outcomes retain
  // the original ID, version, state and body through refreshes and PR tabs.
  useEffect(() => {
    if (intent && !mutation.pending && !uncertain) setIntent(undefined);
  }, [intent, mutation.pending, uncertain]);
  async function run(proposed: Intent): Promise<Saved | undefined> {
    if (busy.current) return;
    const attempt = intent ?? proposed;
    // Prevent an unpostable body from becoming an uncertain retained intent.
    // Issue comments/state requests have a smaller envelope than plain PR comments.
    const bytes = new TextEncoder();
    const limit =
      attempt.kind === "state" || target.startsWith("issues/")
        ? 80 * 1024
        : 160 * 1024;
    if (
      !intent &&
      (bytes.encode(attempt.input.body ?? "").length > 65_536 ||
        bytes.encode(JSON.stringify(attempt.input)).length > limit)
    ) {
      setValidation("This comment is too long. Shorten it before submitting.");
      return;
    }
    setValidation(undefined);
    busy.current = true;
    setIntent(attempt);
    try {
      const url = endpoint(
        repo,
        `${target}/${attempt.kind === "state" ? "state" : "comments"}`,
      );
      let comment: Comment | null;
      if (attempt.kind === "state") {
        const saved = await mutation.run<{ comment: Comment | null }>(
          url,
          "POST",
          attempt.input,
        );
        if (!saved) return;
        comment = saved.comment;
      } else {
        const saved = await mutation.run<Comment>(url, "POST", attempt.input);
        if (!saved) return;
        comment = saved;
      }
      setIntent(undefined);
      if (comment) {
        setCreated(comment);
        setPosted(true);
        const submitted = attempt.input.body;
        setBody((current) => (current === submitted ? "" : current));
        submission({ body: "" });
      }
      return { comment, stateChanged: attempt.kind === "state" };
    } finally {
      busy.current = false;
    }
  }
  return {
    body,
    created,
    posted,
    pending: mutation.pending,
    error: validation ?? mutation.error,
    conflict: mutation.conflict,
    intent: intent?.kind,
    recovering: Boolean(intent && !mutation.pending && uncertain),
    review() {
      if (!uncertain) setIntent(undefined);
      mutation.reset();
    },
    change(value: string) {
      setValidation(undefined);
      setBody(value);
      setPosted(false);
    },
    submit() {
      if (intent?.kind === "state") return Promise.resolve(undefined);
      const input = { body };
      return run({
        kind: "comment",
        input: { ...input, request_id: submission(input) },
      });
    },
    submitState(change: StateChange, mode: "include" | "omit" = "include") {
      if (intent?.kind === "comment") return Promise.resolve(undefined);
      const input = {
        ...change,
        ...(mode === "include" && body.trim() ? { body } : {}),
      };
      return run({
        kind: "state",
        input: { ...input, request_id: submission(input) },
      });
    },
    retry() {
      return intent ? run(intent) : Promise.resolve(undefined);
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
  onStateChanged,
  readOnly = false,
}: {
  comment: ReturnType<typeof useCommentSubmission>;
  id: string;
  actions?: (change: ChangeState) => ReactNode;
  onStateChanged?: () => void;
  readOnly?: boolean;
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
  function complete(saved: Saved | undefined) {
    if (!saved) return;
    // Hidden PR tabs may finish writes without moving the visible history.
    if (saved.comment && form.current?.getClientRects().length)
      completed.current(saved.comment);
    if (saved.stateChanged) onStateChanged?.();
  }
  const change: ChangeState = async (input, mode) =>
    complete(await comment.submitState(input, mode));
  const editor = useReturnFocus<HTMLTextAreaElement>(
    comment.pending && comment.intent === "comment",
  );
  const recovery = useReturnFocus<HTMLButtonElement>(comment.pending);
  return (
    <form
      ref={form}
      className={cn("discussion-compose", className)}
      onSubmit={async (event) => {
        event.preventDefault();
        if (!readOnly) complete(await comment.submit());
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
      {readOnly && (
        <p className="text-sm text-muted-foreground">
          This repository is archived. Your draft remains here to copy.
        </p>
      )}
      {comment.recovering && (
        <div className="flex flex-col items-start gap-2 text-sm" role="status">
          <p>
            The original action may have completed. Recover it before submitting
            another action; any newer draft stays here.
          </p>
          <Button
            ref={recovery}
            type="button"
            variant="outline"
            onClick={async () => complete(await comment.retry())}
          >
            Retry original action
          </Button>
        </div>
      )}
      <div role="status">{comment.posted ? "Comment posted." : ""}</div>
      {comment.posted && comment.created && (
        <Link href={`${href}#issuecomment-${comment.created.number}`}>
          View comment
        </Link>
      )}
      <div className="discussion-actions">
        {actions?.(change)}
        <Button
          type="submit"
          disabled={
            readOnly ||
            busy ||
            comment.pending ||
            comment.intent === "state" ||
            !comment.body.trim()
          }
        >
          {comment.pending ? "Posting…" : "Comment"}
        </Button>
      </div>
    </form>
  );
}
