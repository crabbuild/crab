import { useEffect, useId, useRef, useState } from "react";
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

type SavedSubmission = { destination: string; input: Submission };

function restoreSubmission(
  key: string | undefined,
): SavedSubmission | undefined {
  if (!key) return;
  try {
    const saved = JSON.parse(sessionStorage.getItem(key) ?? "null") as {
      destination?: unknown;
      input?: Partial<Submission>;
    } | null;
    const input = saved?.input;
    if (
      typeof saved?.destination === "string" &&
      typeof input?.request_id === "string" &&
      /^[\da-f]{8}(?:-[\da-f]{4}){3}-[\da-f]{12}$/i.test(input.request_id) &&
      typeof input.expected_head === "string" &&
      /^[\da-f]{40}$/i.test(input.expected_head) &&
      typeof input.message === "string" &&
      input.message.length > 0 &&
      input.message.length <= 256 &&
      Array.isArray(input.threads) &&
      input.threads.length > 0 &&
      input.threads.length <= batchLimit &&
      input.threads.every(
        (thread) =>
          Number.isSafeInteger(thread?.number) &&
          thread.number > 0 &&
          Number.isSafeInteger(thread.version) &&
          thread.version > 0,
      )
    )
      return saved as SavedSubmission;
  } catch {
    // Session storage can be disabled or contain an unreadable submission.
  }
}

function forgetSubmission(key: string | undefined, saved: SavedSubmission) {
  if (!key) return;
  try {
    if (sessionStorage.getItem(key) === JSON.stringify(saved))
      sessionStorage.removeItem(key);
  } catch {
    // A later replay of the same request can only recover the same receipt.
  }
}

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
  number: number,
  pull: PullRequest | undefined,
  csrf: string,
  account: string | null,
) {
  const destination = pull?.head_repository ?? repo;
  const storageKey = account
    ? `crab.suggestion.submission.v1:${JSON.stringify([account, repo.id, number])}`
    : undefined;
  const mutation = useMutation(csrf, destination);
  const trigger = useRef<HTMLElement>(null);
  const [batch, setBatch] = useState<Batch>();
  const [commit, setCommit] = useState<string>();
  const [intent, setIntent] = useState<Batch & { fromBatch: boolean }>();
  const [open, setOpen] = useState(false);
  const [attempt, setAttempt] = useState<
    { saved: SavedSubmission; recovering: boolean } | undefined
  >(() => {
    const saved = restoreSubmission(storageKey);
    return saved ? { saved, recovering: true } : undefined;
  });
  const [message, setMessage] = useState("");
  const enabled = pull?.can_apply_suggestions === true && !repo.archived;
  const comparison = pull && {
    head: pull.head_oid,
    baseRevision: pull.base_revision,
  };
  const current = (value: Batch) =>
    value.head === comparison?.head &&
    value.baseRevision === comparison.baseRevision;
  // A selected thread can leave the mounted page during navigation. Keep
  // its captured version so publication can validate the batch as selected.
  const selected = batch && current(batch) ? batch.threads : [];
  const retry =
    pull &&
    attempt?.saved.destination === destination.id &&
    (attempt.recovering ||
      mutation.status === undefined ||
      mutation.status === 429 ||
      mutation.status >= 500)
      ? attempt.saved.input
      : undefined;
  const rejected =
    mutation.status !== undefined &&
    mutation.status !== 429 &&
    mutation.status < 500;
  useEffect(() => {
    if (attempt && !attempt.recovering && rejected)
      forgetSubmission(storageKey, attempt.saved);
  }, [attempt, rejected, storageKey]);
  function toggle(thread: PullReviewThread) {
    if (!comparison || retry || !canApplySuggestion(thread)) return;
    const next = selected.some((item) => item.number === thread.number)
      ? selected.filter((item) => item.number !== thread.number)
      : [...selected, selection(thread)];
    if (next.length <= batchLimit) setBatch({ ...comparison, threads: next });
  }
  function review(thread?: PullReviewThread) {
    trigger.current =
      document.activeElement instanceof HTMLElement
        ? document.activeElement
        : null;
    if (retry) {
      setOpen(true);
      return;
    }
    if (
      !comparison ||
      !enabled ||
      mutation.pending ||
      (thread && !canApplySuggestion(thread))
    )
      return;
    const threads = thread ? [selection(thread)] : selected;
    if (!threads.length) return;
    mutation.reset();
    setIntent({ ...comparison, threads, fromBatch: thread === undefined });
    setAttempt(undefined);
    setMessage(`Apply review suggestions from pull request #${number}`);
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
  async function apply(onSaved: () => void) {
    if (!pull || mutation.pending) return;
    let input = retry;
    if (!input) {
      if (!intent || stale || !enabled || !message.trim()) return;
      input = {
        request_id: crypto.randomUUID(),
        expected_head: intent.head,
        threads: intent.threads.map(({ number, version }) => ({
          number,
          version,
        })),
        message: message.trim(),
      };
    }
    const saved = { destination: destination.id, input };
    // Save the exact request before publishing. A reload can see a newer
    // head or no live branches, and only the original input can recover it.
    if (storageKey) {
      try {
        sessionStorage.setItem(storageKey, JSON.stringify(saved));
      } catch {
        // The open review still keeps its in-memory retry.
      }
    }
    setAttempt({ saved, recovering: retry !== undefined });
    setCommit(undefined);
    const kind = pull.head_repository ? "upstream-pulls" : "pulls";
    const result = await mutation.run<{ commit: string } | null>(
      endpoint(destination, `${kind}/${number}/suggestions`),
      "POST",
      input,
    );
    if (result?.commit) {
      forgetSubmission(storageKey, saved);
      setCommit(result.commit);
      setBatch(undefined);
      setIntent(undefined);
      setAttempt(undefined);
      setOpen(false);
      onSaved();
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
      if (attempt) forgetSubmission(storageKey, attempt.saved);
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
  onSaved,
}: {
  suggestions: SuggestionBatch;
  branch: string;
  onSaved: () => void;
}) {
  const id = useId();
  const { destination, intent, message, pending, stale, enabled, open, retry } =
    suggestions;
  const count = retry?.threads.length ?? intent?.threads.length ?? 0;
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
                <>
                  Confirm the result of the original submission for{" "}
                  <code>{short(retry.expected_head)}</code>.
                </>
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
              void suggestions.apply(onSaved);
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
                  value={retry?.message ?? message}
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
