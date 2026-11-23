import { useId, useRef, useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Link } from "@/components/navigation/link";
import { endpoint, type Repository } from "@/lib/api";
import { short } from "@/lib/format";
import { repoHref } from "@/lib/repository-route";
import type { PullRequest, PullReviewThread } from "../model";

const batchLimit = 20;

type SelectedSuggestion = Pick<
  PullReviewThread,
  "number" | "version" | "path" | "start_line" | "end_line"
>;

type Batch = {
  head: string;
  baseRevision: PullRequest["base_revision"];
  threads: SelectedSuggestion[];
};

type Submission = {
  request_id: string;
  expected_head: string;
  threads: Array<Pick<SelectedSuggestion, "number" | "version">>;
  message: string;
};

function selection(thread: PullReviewThread): SelectedSuggestion {
  const { number, version, path, start_line, end_line } = thread;
  return { number, version, path, start_line, end_line };
}

export function canApplySuggestion(thread: PullReviewThread) {
  return (
    thread.suggested_text !== null &&
    !!thread.new_blob_oid &&
    thread.side === "new" &&
    !thread.outdated &&
    !thread.vanished &&
    !thread.resolved
  );
}

export function useSuggestionBatch(
  repo: Repository,
  pull: PullRequest,
  csrf: string,
  refresh: () => void,
) {
  const destination = pull.head_repository ?? repo;
  const mutation = useMutation(csrf, destination);
  const trigger = useRef<HTMLElement>(null);
  const [batch, setBatch] = useState<Batch>();
  const [commit, setCommit] = useState<string>();
  const [intent, setIntent] = useState<Batch & { fromBatch: boolean }>();
  const [open, setOpen] = useState(false);
  const [attempt, setAttempt] = useState<{
    input: Submission;
    recovering: boolean;
  }>();
  const [message, setMessage] = useState("");
  const enabled = pull.can_apply_suggestions && !repo.archived;
  const comparison = { head: pull.head_oid, baseRevision: pull.base_revision };
  const current = (value: Batch) =>
    value.head === comparison.head &&
    value.baseRevision === comparison.baseRevision;
  // A selected thread can leave the mounted page during navigation. Keep
  // its captured version so publication can validate the batch as selected.
  const selected = batch && current(batch) ? batch.threads : [];
  const retry =
    attempt &&
    (attempt.recovering ||
      mutation.status === undefined ||
      mutation.status === 429 ||
      mutation.status >= 500)
      ? attempt.input
      : undefined;
  function toggle(thread: PullReviewThread) {
    if (retry || !canApplySuggestion(thread)) return;
    const next = selected.some((item) => item.number === thread.number)
      ? selected.filter((item) => item.number !== thread.number)
      : [...selected, selection(thread)];
    if (next.length <= batchLimit) setBatch({ ...comparison, threads: next });
  }
  function review(thread?: PullReviewThread) {
    if (retry) {
      setOpen(true);
      return;
    }
    if (!enabled || mutation.pending || (thread && !canApplySuggestion(thread)))
      return;
    const threads = thread ? [selection(thread)] : selected;
    if (!threads.length) return;
    mutation.reset();
    trigger.current =
      document.activeElement instanceof HTMLElement
        ? document.activeElement
        : null;
    setIntent({ ...comparison, threads, fromBatch: thread === undefined });
    setAttempt(undefined);
    setMessage(`Apply review suggestions from pull request #${pull.number}`);
    setOpen(true);
  }
  const stale = intent && !current(intent);
  function remove(number: number) {
    if (!intent?.fromBatch || retry || stale || mutation.pending) return;
    const threads = intent.threads.filter((item) => item.number !== number);
    const next = threads.length ? { ...intent, threads } : undefined;
    setAttempt(undefined);
    mutation.reset();
    setBatch(next);
    setIntent(next);
    if (!next) setOpen(false);
  }
  async function apply() {
    if (
      !intent ||
      mutation.pending ||
      (!retry && (stale || !enabled || !message.trim()))
    )
      return;
    const input = retry ?? {
      request_id: crypto.randomUUID(),
      expected_head: intent.head,
      threads: intent.threads.map(({ number, version }) => ({
        number,
        version,
      })),
      message: message.trim(),
    };
    // Retrying always sends the first immutable request, even after a refresh
    // changes the displayed head or the server reports a transient failure.
    setAttempt({ input, recovering: retry !== undefined });
    setCommit(undefined);
    const kind = pull.head_repository ? "upstream-pulls" : "pulls";
    const result = await mutation.run<{ commit: string } | null>(
      endpoint(destination, `${kind}/${pull.number}/suggestions`),
      "POST",
      input,
    );
    if (result?.commit) {
      setCommit(result.commit);
      setBatch(undefined);
      setIntent(undefined);
      setAttempt(undefined);
      setOpen(false);
      refresh();
    }
  }
  return {
    enabled,
    selected: selected.map((thread) => thread.number),
    toggle,
    review,
    remove,
    apply,
    commit,
    destination,
    intent,
    open,
    retry,
    stale,
    message,
    setMessage,
    close() {
      if (mutation.pending) return;
      setOpen(false);
      if (!retry) {
        setIntent(undefined);
        setAttempt(undefined);
      }
    },
    dismissRetry() {
      setIntent(undefined);
      setAttempt(undefined);
      mutation.reset();
    },
    restoreFocus() {
      trigger.current?.focus();
    },
    pending: mutation.pending,
    error:
      mutation.code === "file_changed"
        ? "A selected suggestion changed. Refresh the review and select its current version."
        : retry && !mutation.pending && mutation.status === undefined
          ? "Crab couldn’t confirm the commit. Retry to recover its result."
          : mutation.error,
    atLimit: selected.length >= batchLimit,
  };
}

export type SuggestionBatch = ReturnType<typeof useSuggestionBatch>;

export function SuggestionCommit({
  suggestions,
  branch,
}: {
  suggestions: SuggestionBatch;
  branch: string;
}) {
  const id = useId();
  const { destination, intent, message, pending, stale, enabled, open, retry } =
    suggestions;
  const count = intent?.threads.length ?? 0;
  const error =
    suggestions.error ??
    (stale && !retry
      ? "The review changed. Refresh and select the suggestions again."
      : undefined);
  return (
    <>
      {suggestions.commit && (
        <p role="status">
          Committed suggestions in{" "}
          <Link
            href={repoHref(destination, {
              view: "commit",
              rev: suggestions.commit,
            })}
          >
            <code>{short(suggestions.commit)}</code>
          </Link>
          .
        </p>
      )}
      {retry && !open && (
        <div className="flex flex-wrap gap-2">
          <Button
            variant="outline"
            size="sm"
            onClick={() => suggestions.review()}
          >
            Resume suggestion commit
          </Button>
          <Button variant="ghost" size="sm" onClick={suggestions.dismissRetry}>
            Dismiss retry
          </Button>
        </div>
      )}
      <Dialog
        open={open}
        onOpenChange={(open) => {
          if (!open) suggestions.close();
        }}
      >
        <DialogContent
          showCloseButton={!pending}
          onCloseAutoFocus={(event) => {
            event.preventDefault();
            suggestions.restoreFocus();
          }}
        >
          <DialogHeader>
            <DialogTitle>
              {count === 1
                ? "Commit suggestion"
                : `Commit ${count} suggestions`}
            </DialogTitle>
            <DialogDescription>
              {retry ? (
                "Confirm the result of the original submission."
              ) : (
                <>
                  Create one commit in{" "}
                  <code>
                    {destination.owner}/{destination.name}
                  </code>{" "}
                  on <code>{branch}</code>.
                </>
              )}
            </DialogDescription>
          </DialogHeader>
          <form
            className="flex flex-col gap-4"
            onSubmit={(event) => {
              event.preventDefault();
              void suggestions.apply();
            }}
          >
            {intent?.fromBatch && (
              <ul
                aria-label="Selected suggestions"
                className="max-h-40 divide-y overflow-y-auto"
              >
                {intent.threads.map((thread) => (
                  <li
                    key={thread.number}
                    className="flex items-center justify-between gap-2 py-1.5"
                  >
                    <span className="min-w-0 text-sm">
                      <code className="break-all">{thread.path}</code>
                      <span className="text-muted-foreground">
                        {" "}
                        ·{" "}
                        {thread.start_line === thread.end_line
                          ? `line ${thread.start_line}`
                          : `lines ${thread.start_line}–${thread.end_line}`}
                      </span>
                    </span>
                    <Button
                      type="button"
                      size="sm"
                      variant="ghost"
                      aria-label={`Remove suggestion from thread #${thread.number}`}
                      disabled={pending || stale || !!retry}
                      onClick={() => suggestions.remove(thread.number)}
                    >
                      Remove
                    </Button>
                  </li>
                ))}
              </ul>
            )}
            <FieldGroup>
              <Field>
                <FieldLabel htmlFor={id}>Commit message</FieldLabel>
                <Input
                  id={id}
                  value={message}
                  onChange={(event) =>
                    suggestions.setMessage(event.target.value)
                  }
                  maxLength={256}
                  disabled={pending || !!retry}
                  required
                />
              </Field>
            </FieldGroup>
            {error && (
              <Alert variant="destructive">
                <AlertDescription>{error}</AlertDescription>
              </Alert>
            )}
            <DialogFooter>
              <Button
                type="button"
                variant="outline"
                onClick={suggestions.close}
                disabled={pending}
              >
                {retry ? "Back to review" : "Cancel"}
              </Button>
              <Button
                type="submit"
                disabled={
                  pending || (!retry && (stale || !enabled || !message.trim()))
                }
              >
                {pending
                  ? "Committing…"
                  : retry
                    ? "Retry suggestion commit"
                    : "Commit changes"}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
    </>
  );
}
