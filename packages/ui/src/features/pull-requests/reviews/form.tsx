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
import type { PullRequest, PullReview, ReviewState } from "../model";

export interface ReviewDraft {
  body: string;
  state: ReviewState;
  commit_oid: string;
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
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  refresh: () => void;
  draft: ReviewDraft | undefined;
  onDraft: (draft: ReviewDraft | undefined) => void;
  submission: (input: object) => string;
}) {
  const [open, setOpen] = useState(false);
  const id = useId();
  const mutation = useMutation(csrf);
  const stale = !!draft && draft.commit_oid !== pull.head_oid;
  return (
    <div className="flex justify-end py-3">
      <Dialog
        open={open}
        onOpenChange={(next) => {
          if (next && !draft)
            onDraft({
              body: "",
              state: "commented",
              commit_oid: pull.head_oid,
            });
          setOpen(next);
        }}
      >
        <DialogTrigger asChild>
          <Button aria-label="Review changes">Review changes</Button>
        </DialogTrigger>
        {open && draft && (
          <DialogContent className="max-h-[calc(100svh-2rem)] overflow-y-auto sm:max-w-xl">
            <DialogHeader>
              <DialogTitle>Submit your review</DialogTitle>
              <DialogDescription>
                Reviewing commit <code>{short(draft.commit_oid)}</code>.
              </DialogDescription>
            </DialogHeader>
            <form
              className="flex flex-col gap-4"
              onSubmit={async (event) => {
                event.preventDefault();
                const input = {
                  body: draft.body,
                  state: draft.state,
                  commit_oid: draft.commit_oid,
                };
                const created = await mutation.run<PullReview>(
                  endpoint(repo, `pulls/${pull.number}/reviews`),
                  "POST",
                  { ...input, request_id: submission(input) },
                );
                if (created) {
                  onDraft(undefined);
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
                disabled={mutation.pending}
                required={draft.state !== "approved"}
                autoFocus
              />
              <RadioGroup
                aria-label="Review decision"
                value={draft.state}
                disabled={mutation.pending}
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
                        mutation.pending ||
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
              {stale && (
                <Alert>
                  <AlertDescription>
                    This draft targets an earlier commit. Review the updated
                    comparison before applying your draft to the current commit.
                  </AlertDescription>
                </Alert>
              )}
              <div className="flex flex-wrap justify-end gap-2">
                {mutation.conflict && (
                  <Button type="button" variant="outline" onClick={refresh}>
                    Reload comparison
                  </Button>
                )}
                {stale && (
                  <Button
                    type="button"
                    variant="outline"
                    onClick={() => {
                      onDraft({ ...draft, commit_oid: pull.head_oid });
                      mutation.reset();
                    }}
                  >
                    Use current comparison
                  </Button>
                )}
                <Button
                  type="submit"
                  disabled={
                    mutation.pending ||
                    mutation.conflict ||
                    stale ||
                    (draft.state !== "commented" && !pull.can_decide) ||
                    (draft.state !== "approved" && !draft.body.trim())
                  }
                >
                  {mutation.pending ? "Submitting…" : "Submit review"}
                </Button>
              </div>
            </form>
          </DialogContent>
        )}
      </Dialog>
    </div>
  );
}
