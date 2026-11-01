import { useState, type FormEvent, type ReactNode } from "react";
import {
  InputGroup,
  InputGroupInput,
  InputGroupAddon,
} from "@/components/ui/input-group";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  Field,
  FieldContent,
  FieldDescription,
  FieldGroup,
  FieldLabel,
  FieldLegend,
  FieldSet,
} from "@/components/ui/field";
import { GitBranchIcon } from "@primer/octicons-react";
import type { Repository } from "@/lib/api";

type CommitTarget = {
  mode: "direct" | "pull";
  setMode: (mode: "direct" | "pull") => void;
  newBranch: string;
  setNewBranch: (branch: string) => void;
  protected: boolean;
};

export function useCommitTarget(
  repo: Repository,
  branch: string,
): CommitTarget {
  const protectedBranch = repo.protected_branches.some(
    (rule) => `refs/heads/${rule.branch}` === branch,
  );
  const [mode, setMode] = useState<"direct" | "pull">(
    protectedBranch ? "pull" : "direct",
  );
  const [newBranch, setNewBranch] = useState("");
  return {
    mode,
    setMode,
    newBranch,
    setNewBranch,
    protected: protectedBranch,
  };
}

export function proposedBranch(target: CommitTarget) {
  return target.mode === "pull" ? target.newBranch.trim() : undefined;
}

type CommitProps = {
  message: string;
  setMessage: (message: string) => void;
  placeholder: string;
  saving: boolean;
  error?: string;
  cancel: () => void;
  branch: string;
  target: CommitTarget;
  danger?: boolean;
};

function CommitFields({
  message,
  setMessage,
  placeholder,
  saving,
  error,
  cancel,
  branch,
  target,
  danger = false,
}: CommitProps) {
  return (
    <>
      <FieldGroup>
        <Field>
          <FieldLabel htmlFor="commit-message">Commit message</FieldLabel>
          <Input
            id="commit-message"
            placeholder={placeholder}
            value={message}
            required
            onChange={(event) => setMessage(event.target.value)}
          />
        </Field>
      </FieldGroup>
      <FieldSet className="commit-target" disabled={saving}>
        <FieldLegend id="commit-target-heading" variant="label">
          Choose a branch for your changes
        </FieldLegend>
        <FieldGroup
          className="gap-3"
          role="radiogroup"
          aria-labelledby="commit-target-heading"
        >
          <Field orientation="horizontal" data-disabled={target.protected}>
            <input
              id="commit-direct"
              className="mt-0.5 size-4 shrink-0"
              type="radio"
              name="commit-target"
              value="direct"
              checked={target.mode === "direct"}
              disabled={target.protected}
              onChange={() => target.setMode("direct")}
              aria-describedby={
                target.protected ? "commit-protected" : undefined
              }
            />
            <FieldContent>
              <FieldLabel htmlFor="commit-direct">
                Commit directly to {branch.slice("refs/heads/".length)}
              </FieldLabel>
              {target.protected && (
                <FieldDescription id="commit-protected">
                  This branch requires changes through a pull request.
                </FieldDescription>
              )}
            </FieldContent>
          </Field>
          <Field orientation="horizontal">
            <input
              id="commit-pull"
              className="mt-0.5 size-4 shrink-0"
              type="radio"
              name="commit-target"
              value="pull"
              checked={target.mode === "pull"}
              onChange={() => target.setMode("pull")}
              aria-describedby="commit-pull-description"
            />
            <FieldContent>
              <FieldLabel htmlFor="commit-pull">
                Create a new branch for this commit and start a pull request
              </FieldLabel>
              <FieldDescription id="commit-pull-description">
                You can review the diff before opening the pull request.
              </FieldDescription>
            </FieldContent>
          </Field>
        </FieldGroup>
        {target.mode === "pull" && (
          <div className="commit-branch-name">
            <InputGroup>
              <InputGroupInput
                aria-label="New branch name"
                placeholder="my-change"
                value={target.newBranch}
                required
                onChange={(event) => target.setNewBranch(event.target.value)}
              />
              <InputGroupAddon>
                <GitBranchIcon aria-hidden="true" />
              </InputGroupAddon>
            </InputGroup>
          </div>
        )}
      </FieldSet>
      {error && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
      <div className="create-file-actions">
        <Button
          variant={
            danger && target.mode === "direct" ? "destructive" : "default"
          }
          type="submit"
          disabled={
            saving || (target.mode === "pull" && !target.newBranch.trim())
          }
        >
          {saving
            ? target.mode === "pull"
              ? "Proposing…"
              : "Committing…"
            : target.mode === "pull"
              ? "Propose changes"
              : "Commit changes"}
        </Button>
        <Button
          variant="outline"
          type="button"
          disabled={saving}
          onClick={cancel}
        >
          Cancel changes
        </Button>
      </div>
    </>
  );
}

export function CommitPanel(props: CommitProps) {
  return (
    <section className="create-file-commit" aria-labelledby="commit-heading">
      <h3 id="commit-heading">Commit changes</h3>
      <CommitFields {...props} />
    </section>
  );
}

export function CommitDialog({
  children,
  open,
  onOpenChange,
  onSubmit,
  filename,
  ...props
}: Omit<CommitProps, "cancel"> & {
  children: ReactNode;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSubmit: (event: FormEvent) => void;
  filename: string;
}) {
  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!props.saving) onOpenChange(next);
      }}
    >
      {children}
      <DialogContent
        className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-xl"
        showCloseButton={!props.saving}
      >
        <DialogHeader>
          <DialogTitle>Commit changes</DialogTitle>
          <DialogDescription>
            Save {filename} to the repository.
          </DialogDescription>
        </DialogHeader>
        <form onSubmit={onSubmit}>
          <CommitFields {...props} cancel={() => onOpenChange(false)} />
        </form>
      </DialogContent>
    </Dialog>
  );
}
