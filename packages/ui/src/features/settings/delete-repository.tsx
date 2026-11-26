import { useRef, useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { navigate, useRequest, type Repository } from "@/lib/api";

type Eligibility = {
  version: number;
  can_delete: boolean;
  forks: number;
  public_forks: number;
};

export function DeleteRepositorySettings({
  repo,
  csrf,
  onChanged,
}: {
  repo: Repository;
  csrf: string;
  onChanged: () => void;
}) {
  const [open, setOpen] = useState(false);
  const path = `/api/repositories/${repo.id}/deletion`;
  const eligibility = useRequest<Eligibility>(open ? path : null);
  return (
    <div className="danger-zone-row">
      <div>
        <strong>Delete this repository</strong>
        <p>Remove it from the organization’s active repositories.</p>
      </div>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogTrigger asChild>
          <Button variant="destructive" size="sm">
            Delete this repository
          </Button>
        </DialogTrigger>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Delete repository</DialogTitle>
            <DialogDescription>
              Deletion removes access to the repository and clears team grants.
            </DialogDescription>
          </DialogHeader>
          <Result state={eligibility} timingLabel="Repository deletion">
            {(state) =>
              state.can_delete ? (
                <DeleteForm
                  repo={repo}
                  csrf={csrf}
                  path={path}
                  eligibility={state}
                  onChanged={onChanged}
                />
              ) : (
                <p className="text-sm text-muted-foreground">
                  Your organization’s policy restricts who can delete
                  repositories. Contact an organization owner.
                </p>
              )
            }
          </Result>
        </DialogContent>
      </Dialog>
    </div>
  );
}

function DeleteForm({
  repo,
  csrf,
  path,
  eligibility,
  onChanged,
}: {
  repo: Repository;
  csrf: string;
  path: string;
  eligibility: Eligibility;
  onChanged: () => void;
}) {
  const [confirmation, setConfirmation] = useState("");
  const submission = useRef<string | undefined>(undefined);
  const mutation = useMutation(csrf, repo);
  const name = `${repo.owner}/${repo.name}`;
  return (
    <form
      className="grid gap-4"
      onSubmit={async (event) => {
        event.preventDefault();
        if (confirmation !== name || mutation.pending) return;
        submission.current ??= crypto.randomUUID();
        const result = await mutation.run<{ organization: string }>(
          path,
          "DELETE",
          {
            request_id: submission.current,
            version: eligibility.version,
            confirmation,
          },
        );
        if (result) {
          onChanged();
          navigate(
            `/-/organizations/${encodeURIComponent(result.organization)}?tab=repositories`,
          );
        }
      }}
    >
      {repo.fork ? (
        <p className="text-sm">
          The upstream repository stays available. This fork can’t be restored
          separately.
        </p>
      ) : (
        <p className="text-sm">
          {eligibility.forks > 0 &&
            `This also deletes ${eligibility.forks} private ${eligibility.forks === 1 ? "fork" : "forks"}. `}
          {eligibility.public_forks > 0 &&
            "This repository’s forks will stay public. The oldest will become their upstream. "}
          Organization owners can restore the repository for 90 days.
        </p>
      )}
      <Field>
        <FieldLabel
          htmlFor="repository-deletion-confirmation"
          className="wrap-anywhere"
        >
          Type {name} to confirm deletion
        </FieldLabel>
        <Input
          id="repository-deletion-confirmation"
          value={confirmation}
          onChange={(event) => setConfirmation(event.target.value)}
          autoComplete="off"
          spellCheck={false}
        />
      </Field>
      {mutation.error && (
        <p role="alert" className="text-destructive">
          {mutation.error}
        </p>
      )}
      <Button
        type="submit"
        variant="destructive"
        disabled={confirmation !== name || mutation.pending}
      >
        {mutation.pending ? "Deleting repository…" : "Delete repository"}
      </Button>
    </form>
  );
}
