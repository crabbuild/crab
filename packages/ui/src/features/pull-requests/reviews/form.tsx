import { useId, useState } from "react";
import { endpoint, navigate, type Repository } from "@/lib/api";
import { Editor } from "@/components/discussion/editor";
import { useMutation } from "@/components/discussion/mutations";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import {
  Field,
  FieldContent,
  FieldDescription,
  FieldError,
  FieldLabel,
} from "@/components/ui/field";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { repoHref } from "@/lib/repository-route";
import { short } from "@/lib/format";
import type { PullRequest } from "../model";
import type { PendingReview } from "./pending";
import type {
  Review,
  ReviewState,
} from "@/components/discussion/conversation/model";

export interface ReviewDraft {
  body: string;
  state: ReviewState;
  commit_oid: string;
  base_revision?: number;
  attempt?: {
    expected: string;
    request_id: string;
    body: string;
    state: ReviewState;
    base_revision?: number;
  };
}
const decisions: Array<{
  value: ReviewState;
  label: string;
  description: string;
}> = [
  {
    value: "commented",
    label: "Comment",
    description: "Leave feedback without an approval decision.",
  },
  {
    value: "approved",
    label: "Approve",
    description: "Accept the changes at the inspected commit.",
  },
  {
    value: "changes_requested",
    label: "Request changes",
    description: "Flag concerns that should be addressed before merging.",
  },
];

export function ReviewChanges({
  repo,
  pull,
  csrf,
  refresh,
  draft,
  onDraft,
  submission,
  disabled,
  pendingReview,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  refresh: () => void;
  draft: ReviewDraft | undefined;
  onDraft: (draft: ReviewDraft | undefined) => void;
  submission: (input: object) => string;
  disabled: boolean;
  pendingReview: PendingReview;
}) {
  const [open, setOpen] = useState(false);
  const id = useId();
  const mutation = useMutation(csrf, repo);
  const pending = pendingReview.draft;
  const count = pending?.comments.length ?? 0;
  const anchoredHead = pending?.comments[0]?.head_oid;
  const attempt = draft?.attempt;
  const staleHead =
    !!draft && (anchoredHead ?? draft.commit_oid) !== pull.head_oid;
  const staleBase =
    !!draft && draft.base_revision !== (pull.base_revision ?? undefined);
  const stale = staleHead || staleBase;
  const rejected =
    !!attempt &&
    (mutation.rejected ||
      [400, 403, 404, 409, 413, 422, 423].some(
        (status) => status === mutation.status,
      ));
  const requiresSummary = !count && draft?.state !== "approved";
  const inputsDisabled = mutation.pending || !!attempt;
  const canSubmit =
    !!draft &&
    !mutation.pending &&
    !mutation.conflict &&
    !rejected &&
    (!!attempt ||
      (!disabled &&
        pendingReview.ready &&
        !pendingReview.pending &&
        !stale &&
        (draft.state === "commented" || pull.can_decide) &&
        (!requiresSummary || !!draft.body.trim())));
  return (
    <div className="flex justify-end py-3">
      <Dialog
        open={open}
        onOpenChange={(next) => {
          if (next && !draft)
            onDraft({
              body: "",
              state: "commented",
              commit_oid: anchoredHead ?? pull.head_oid,
              base_revision: pull.base_revision ?? undefined,
            });
          setOpen(next);
        }}
      >
        <DialogTrigger asChild>
          <Button
            disabled={
              !attempt &&
              (disabled || !pendingReview.ready || pendingReview.pending)
            }
            aria-label="Review changes"
          >
            {attempt
              ? "Resume review submission"
              : count
                ? `Review changes (${count})`
                : "Review changes"}
          </Button>
        </DialogTrigger>
        {open && draft && (
          <DialogContent className="max-h-[calc(100svh-2rem)] overflow-y-auto sm:max-w-xl">
            <DialogHeader>
              <DialogTitle>Submit your review</DialogTitle>
              <DialogDescription>
                Reviewing commit{" "}
                <code>{short(anchoredHead ?? draft.commit_oid)}</code>.
                {count > 0 &&
                  ` This will publish ${count} pending ${count === 1 ? "comment" : "comments"}.`}
              </DialogDescription>
            </DialogHeader>
            <form
              className="flex flex-col gap-4"
              onSubmit={async (event) => {
                event.preventDefault();
                if (!canSubmit) return;
                const { body, state, commit_oid, base_revision } = draft;
                const version = count ? pending?.version : null;
                const batch =
                  attempt ??
                  (version
                    ? {
                        expected: version,
                        request_id: crypto.randomUUID(),
                        body,
                        state,
                        base_revision,
                      }
                    : undefined);
                if (batch && !attempt) onDraft({ ...draft, attempt: batch });
                const input = { body, state, commit_oid, base_revision };
                const resource = batch ? "pending-review/submit" : "reviews";
                const created = await mutation.run<
                  Review | { review: number; threads: number[] }
                >(
                  endpoint(repo, `pulls/${pull.number}/${resource}`),
                  "POST",
                  batch ?? { ...input, request_id: submission(input) },
                );
                if (created) {
                  onDraft(undefined);
                  if (batch) pendingReview.reload();
                  setOpen(false);
                  refresh();
                  navigate(
                    repoHref(repo, {
                      view: "pulls",
                      pull: String(pull.number),
                    }),
                  );
                }
              }}
            >
              <Editor
                id="pull-review"
                label="Review summary"
                value={draft.body}
                onChange={(body) => onDraft({ ...draft, body })}
                disabled={inputsDisabled}
                required={!attempt && requiresSummary}
                autoFocus
              />
              {pull.review_counts_for_merge === false &&
                pull.merge_requirements.required_approvals > 0 && (
                  <p className="muted">
                    Required reviews count approvals from repository writers and
                    administrators.
                  </p>
                )}
              <RadioGroup
                aria-label="Review decision"
                value={draft.state}
                disabled={inputsDisabled}
                onValueChange={(state) => {
                  if (
                    state === "commented" ||
                    state === "approved" ||
                    state === "changes_requested"
                  )
                    onDraft({ ...draft, state });
                }}
              >
                {decisions.map((decision) => (
                  <Field
                    key={decision.value}
                    orientation="horizontal"
                    data-disabled={
                      decision.value !== "commented" && !pull.can_decide
                    }
                  >
                    <RadioGroupItem
                      id={`${id}-${decision.value}`}
                      value={decision.value}
                      aria-label={decision.label}
                      disabled={
                        inputsDisabled ||
                        (decision.value !== "commented" && !pull.can_decide)
                      }
                    />
                    <FieldContent>
                      <FieldLabel htmlFor={`${id}-${decision.value}`}>
                        {decision.label}
                      </FieldLabel>
                      <FieldDescription>
                        {decision.description}
                      </FieldDescription>
                    </FieldContent>
                  </Field>
                ))}
              </RadioGroup>
              {!pull.can_decide && (
                <p className="text-muted-foreground">
                  Authors can comment, but cannot decide on their own changes.
                </p>
              )}
              {mutation.error && <FieldError>{mutation.error}</FieldError>}
              {stale && !attempt && (
                <Alert>
                  <AlertDescription>
                    {staleBase
                      ? "The base branch changed. Review the updated comparison before submitting."
                      : count
                        ? "The pending comments target an earlier commit. Review the updated diff before adding new comments."
                        : "This draft targets an earlier commit. Review the updated comparison before applying your draft to the current commit."}
                  </AlertDescription>
                </Alert>
              )}
              <div className="flex flex-wrap justify-end gap-2">
                {(mutation.conflict || (attempt && rejected)) && (
                  <Button
                    type="button"
                    variant="outline"
                    onClick={() => {
                      mutation.reset();
                      if (attempt) {
                        onDraft({ ...draft, attempt: undefined });
                        pendingReview.reload();
                      }
                      setOpen(false);
                      refresh();
                    }}
                  >
                    Reload comparison
                  </Button>
                )}
                {stale && (!count || !staleHead) && !attempt && (
                  <Button
                    type="button"
                    variant="outline"
                    onClick={() => {
                      onDraft({
                        ...draft,
                        commit_oid: pull.head_oid,
                        base_revision: pull.base_revision ?? undefined,
                      });
                      mutation.reset();
                    }}
                  >
                    Use current comparison
                  </Button>
                )}
                <Button type="submit" disabled={!canSubmit}>
                  {mutation.pending
                    ? "Submitting…"
                    : attempt
                      ? "Retry review submission"
                      : "Submit review"}
                </Button>
              </div>
            </form>
          </DialogContent>
        )}
      </Dialog>
    </div>
  );
}
