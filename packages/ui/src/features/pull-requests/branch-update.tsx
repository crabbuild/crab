import { ChevronDownIcon, SyncIcon } from "@primer/octicons-react";
import { useRef, useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { short } from "@/lib/format";
import type { PullRequest } from "./model";
import { branch } from "./presentation";
import { ResolveConflicts } from "./resolve-conflicts";

export interface UpdateInput {
  request_id: string;
  version: number;
  base_oid: string;
  head_oid: string;
}

type Submission = UpdateInput & { method?: "rebase" };

export function BranchUpdate({
  repo,
  pull,
  csrf,
  active,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  active: boolean;
  refresh: () => void;
}) {
  const head = pull.head_repository ?? repo;
  const scope = pull.head_repository ? "upstream-pulls" : "pulls";
  const baseName = pull.head_repository
    ? `${repo.name}:${branch(pull.base_ref)}`
    : branch(pull.base_ref);
  const headName = pull.head_repository
    ? `${head.name}:${branch(pull.head_ref)}`
    : branch(pull.head_ref);
  const action = `${scope}/${pull.number}/update-branch`;
  const url = endpoint(head, action);
  const preview = useRequest<{
    update: UpdateInput | null;
    rebase_blocked?: boolean;
  }>(active ? url : null);
  const mutation = useMutation(csrf, head);
  const [missingReply, setMissingReply] = useState(false);
  const [attempt, setAttempt] = useState<Submission>();
  const [confirmation, setConfirmation] = useState<UpdateInput>();
  const panel = useRef<HTMLElement>(null);
  const options = useRef<HTMLButtonElement>(null);
  const retry =
    attempt && (mutation.status === undefined || mutation.status >= 500)
      ? attempt
      : undefined;
  const failure =
    mutation.error ??
    (missingReply
      ? "The response was lost. Retry this update to recover a possible completed write."
      : preview.error);
  const update = preview.data?.update;
  const rebaseBlocked = preview.data?.rebase_blocked === true;
  const changed =
    update &&
    (update.base_oid !== pull.base_oid ||
      update.head_oid !== pull.head_oid ||
      update.version !== pull.version);
  if (!update && !failure && !retry) return null;

  function refreshed() {
    mutation.reset();
    setMissingReply(false);
    setAttempt(undefined);
    setConfirmation(undefined);
    preview.retry();
    refresh();
  }

  async function apply(input: Submission) {
    setAttempt(input);
    setMissingReply(false);
    const saved = await mutation.run<{ commit: string } | null>(
      url,
      "POST",
      input,
    );
    setMissingReply(saved !== undefined && !saved?.commit);
    if (saved?.commit) {
      refreshed();
    }
  }

  return (
    <section
      ref={panel}
      aria-label="Update pull request branch"
      tabIndex={-1}
      className="grid gap-2 rounded-md border p-4 sm:grid-cols-[1fr_auto] sm:items-start"
    >
      <div className="min-w-0 text-sm">
        {update && (
          <p className="break-words">
            Bring changes from <code>{baseName}</code> at{" "}
            <code>{short(update.base_oid)}</code> into <code>{headName}</code>.
          </p>
        )}
        {changed && (
          <p role="alert">The comparison changed. Refresh before updating.</p>
        )}
        {update && rebaseBlocked && <p>A branch rule blocks rebasing here.</p>}
        {failure && (
          <p role="alert" className="text-destructive">
            {failure}
          </p>
        )}
      </div>
      <div className="flex flex-wrap gap-2">
        {update && !changed && mutation.code === "branch_update_conflict" ? (
          <ResolveConflicts
            url={url}
            previewUrl={endpoint(head, action, { conflicts: "true" })}
            head={head}
            suggestedBranch={`resolve-pr-${pull.number}`}
            csrf={csrf}
            comparison={update}
            refresh={refreshed}
          />
        ) : retry ? (
          <Button
            variant="outline"
            size="sm"
            disabled={mutation.pending}
            onClick={() => void apply(retry)}
          >
            <SyncIcon />
            {retry.method === "rebase"
              ? mutation.pending
                ? "Rebasing…"
                : "Retry rebase"
              : mutation.pending
                ? "Updating…"
                : "Update branch"}
          </Button>
        ) : update && !changed ? (
          <div className="inline-flex">
            <Button
              variant="outline"
              size="sm"
              className={rebaseBlocked ? undefined : "rounded-r-none"}
              onClick={() => void apply(update)}
            >
              <SyncIcon /> Update branch
            </Button>
            {!rebaseBlocked && (
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button
                    ref={options}
                    variant="outline"
                    size="sm"
                    className="rounded-l-none border-l-0 px-2"
                    aria-label="Branch update options"
                  >
                    <ChevronDownIcon />
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent
                  align="end"
                  onCloseAutoFocus={(event) => {
                    if (confirmation) event.preventDefault();
                  }}
                >
                  <DropdownMenuItem onSelect={() => setConfirmation(update)}>
                    Update with rebase
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            )}
          </div>
        ) : null}
        {(changed || failure) && (
          <Button
            variant="ghost"
            size="sm"
            disabled={mutation.pending}
            onClick={refreshed}
          >
            Refresh comparison
          </Button>
        )}
      </div>
      <Dialog
        open={!!confirmation}
        onOpenChange={(next) => {
          if (!next) setConfirmation(undefined);
        }}
      >
        <DialogContent
          onCloseAutoFocus={(event) => {
            event.preventDefault();
            (options.current ?? panel.current)?.focus();
          }}
        >
          <DialogHeader>
            <DialogTitle>Rebase branch</DialogTitle>
            <DialogDescription>
              Replay <code>{headName}</code> at{" "}
              <code>{short(confirmation?.head_oid ?? "")}</code> onto{" "}
              <code>{baseName}</code> at{" "}
              <code>{short(confirmation?.base_oid ?? "")}</code>. This rewrites
              the head branch history and changes its commit IDs.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setConfirmation(undefined)}
            >
              Cancel
            </Button>
            <Button
              disabled={changed || rebaseBlocked || mutation.pending}
              onClick={() => {
                if (!confirmation) return;
                setConfirmation(undefined);
                void apply({ ...confirmation, method: "rebase" });
              }}
            >
              Rebase branch
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </section>
  );
}
