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
  threads: PullReviewThread[],
  refresh: () => void,
) {
  const destination = pull.head_repository ?? repo;
  const mutation = useMutation(csrf, destination);
  const trigger = useRef<HTMLElement>(null);
  const [batch, setBatch] = useState<{
    head: string;
    threads: Array<Pick<PullReviewThread, "number" | "version">>;
  }>();
  const [commit, setCommit] = useState<string>();
  const [intent, setIntent] = useState<{
    head: string;
    threads: Array<Pick<PullReviewThread, "number" | "version">>;
  }>();
  const [message, setMessage] = useState("");
  const enabled = pull.can_apply_suggestions && !repo.archived;
  const selected =
    batch?.head === pull.head_oid
      ? batch.threads.filter((item) =>
          threads.some(
            (thread) =>
              thread.number === item.number &&
              thread.version === item.version &&
              canApplySuggestion(thread),
          ),
        )
      : [];
  function toggle(number: number) {
    const thread = threads.find((item) => item.number === number);
    if (!thread || !canApplySuggestion(thread)) return;
    const next = selected.some((item) => item.number === number)
      ? selected.filter((item) => item.number !== number)
      : [...selected, { number, version: thread.version }];
    if (next.length <= batchLimit)
      setBatch({ head: pull.head_oid, threads: next });
  }
  function review(ids: number[]) {
    if (!enabled || mutation.pending || !ids.length) return;
    const selectedThreads = ids.flatMap((id) => {
      const thread = threads.find((item) => item.number === id);
      return thread && canApplySuggestion(thread)
        ? [{ number: thread.number, version: thread.version }]
        : [];
    });
    if (selectedThreads.length !== ids.length) return;
    mutation.reset();
    trigger.current =
      document.activeElement instanceof HTMLElement
        ? document.activeElement
        : null;
    setIntent({ head: pull.head_oid, threads: selectedThreads });
    setMessage(`Apply review suggestions from pull request #${pull.number}`);
  }
  const stale =
    intent &&
    (intent.head !== pull.head_oid ||
      intent.threads.some(
        ({ number, version }) =>
          !threads.some(
            (thread) =>
              thread.number === number &&
              thread.version === version &&
              canApplySuggestion(thread),
          ),
      ));
  async function apply() {
    if (!intent || stale || !enabled || mutation.pending || !message.trim())
      return;
    setCommit(undefined);
    const kind = pull.head_repository ? "upstream-pulls" : "pulls";
    const result = await mutation.run<{ commit: string }>(
      endpoint(destination, `${kind}/${pull.number}/suggestions`),
      "POST",
      {
        expected_head: intent.head,
        threads: intent.threads,
        message: message.trim(),
      },
    );
    if (result) {
      setCommit(result.commit);
      setBatch(undefined);
      setIntent(undefined);
      refresh();
    }
  }
  return {
    enabled,
    selected: selected.map((thread) => thread.number),
    toggle,
    review,
    apply,
    commit,
    destination,
    intent,
    stale,
    message,
    setMessage,
    close() {
      if (!mutation.pending) setIntent(undefined);
    },
    restoreFocus() {
      trigger.current?.focus();
    },
    pending: mutation.pending,
    error:
      mutation.error && mutation.status === undefined
        ? "Crab couldn’t confirm the commit. Refresh the branch before trying again."
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
  const { destination, intent, message, pending, stale, enabled } = suggestions;
  const count = intent?.threads.length ?? 0;
  const error =
    suggestions.error ??
    (stale
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
      <Dialog
        open={Boolean(intent)}
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
              Create one commit in{" "}
              <code>
                {destination.owner}/{destination.name}
              </code>{" "}
              on <code>{branch}</code>.
            </DialogDescription>
          </DialogHeader>
          <form
            className="flex flex-col gap-4"
            onSubmit={(event) => {
              event.preventDefault();
              void suggestions.apply();
            }}
          >
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
                  disabled={pending}
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
                Cancel
              </Button>
              <Button
                type="submit"
                disabled={pending || stale || !enabled || !message.trim()}
              >
                {pending ? "Committing…" : "Commit changes"}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
    </>
  );
}
