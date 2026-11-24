import { FieldSelect } from "@/components/ui/field-select";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Field, FieldLabel } from "@/components/ui/field";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import {
  ChangeComparison,
  usePullComparison,
} from "@/features/repository/diff";
import { endpoint, useRequest, type Refs, type Repository } from "@/lib/api";
import type { CodeThemes } from "@/lib/code-theme";
import { short } from "@/lib/format";
import type { PullRequest } from "./model";
import { branch } from "./presentation";

interface Change {
  request_id: string;
  version: number;
  base_ref: string;
  base_oid: string;
  head_oid: string;
}

export function ChangeBase({
  repo,
  pull,
  csrf,
  theme,
  codeThemes,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  refresh: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [selection, setSelection] = useState("");
  const [attempt, setAttempt] = useState<Change>();
  const [missingReply, setMissingReply] = useState(false);
  const refs = useRequest<Refs>(open ? endpoint(repo, "refs") : null);
  const mutation = useMutation(csrf, repo);
  const headRepo = pull.head_repository ?? repo;
  const eligible =
    selection &&
    selection !== pull.base_ref &&
    (pull.head_repository || selection !== pull.head_ref);
  const comparison = usePullComparison(
    headRepo,
    open && eligible ? selection : "",
    pull.head_ref,
    pull.head_repository ? repo.id : undefined,
  );
  const preview = comparison.data;
  const rejected =
    !!attempt &&
    mutation.status !== undefined &&
    [400, 403, 404, 409, 422].includes(mutation.status);

  async function changeBase() {
    if (rejected) return;
    const input =
      attempt ??
      (preview && {
        request_id: crypto.randomUUID(),
        version: pull.version,
        base_ref: selection,
        base_oid: preview.base_tip,
        head_oid: preview.commit,
      });
    if (!input) return;
    setAttempt(input);
    setMissingReply(false);
    const result = await mutation.run<{ saved: true } | null>(
      endpoint(repo, `pulls/${pull.number}/base`),
      "POST",
      input,
    );
    if (result?.saved) {
      setAttempt(undefined);
      setSelection("");
      setOpen(false);
      refresh();
    } else if (result === null) setMissingReply(true);
  }

  function reviewAgain() {
    setAttempt(undefined);
    mutation.reset();
    setMissingReply(false);
    refs.retry();
    comparison.retry();
    refresh();
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!mutation.pending) setOpen(next);
      }}
    >
      <DialogTrigger asChild>
        <Button variant="outline" size="sm">
          Change base
        </Button>
      </DialogTrigger>
      <DialogContent className="max-h-[calc(100svh-2rem)] overflow-y-auto sm:max-w-4xl">
        <DialogHeader>
          <DialogTitle>Change the base branch</DialogTitle>
          <DialogDescription>
            The pull request currently targets{" "}
            <code>{branch(pull.base_ref)}</code>. Changing the base can make
            review comments outdated and requires fresh approvals.
          </DialogDescription>
        </DialogHeader>
        <Result state={refs}>
          {({ refs: rows }) => (
            <Field>
              <FieldLabel htmlFor="change-pull-base">
                New base branch
              </FieldLabel>
              <FieldSelect
                id="change-pull-base"
                value={selection}
                disabled={!!attempt || mutation.pending}
                onValueChange={(nextValue) => setSelection(nextValue)}
                options={[
                  { value: "", label: "Choose a branch" },
                  ...rows
                    .filter(
                      ({ name }) =>
                        name.startsWith("refs/heads/") &&
                        name !== pull.base_ref &&
                        (pull.head_repository || name !== pull.head_ref),
                    )
                    .map(({ name }) => ({ value: name, label: branch(name) })),
                ]}
              />
            </Field>
          )}
        </Result>
        {eligible && (
          <section aria-label="New comparison" className="min-w-0 space-y-2">
            {preview && (
              <p className="text-sm text-muted-foreground">
                Base <code>{short(preview.base_tip)}</code> · Head{" "}
                <code>{short(preview.commit)}</code>
              </p>
            )}
            <ChangeComparison
              idPrefix="base-preview-"
              state={comparison}
              repo={headRepo}
              rev={preview?.commit ?? pull.head_ref}
              base={preview?.base}
              theme={theme}
              codeThemes={codeThemes}
            />
          </section>
        )}
        {(mutation.error || missingReply) && (
          <p role="alert" className="text-sm text-destructive">
            {mutation.error ??
              "The reply was lost. Retry the original change to recover its result."}
          </p>
        )}
        <DialogFooter>
          <Button
            variant="outline"
            disabled={mutation.pending}
            onClick={() => setOpen(false)}
          >
            Cancel
          </Button>
          {rejected ? (
            <Button onClick={reviewAgain}>Refresh comparison</Button>
          ) : (
            <Button
              disabled={
                mutation.pending ||
                (!attempt && (!eligible || comparison.loading || !preview))
              }
              onClick={() => void changeBase()}
            >
              {mutation.pending
                ? "Changing base…"
                : attempt
                  ? "Retry base change"
                  : "Confirm base change"}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
