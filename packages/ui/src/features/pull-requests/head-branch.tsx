import { useEffect, useState } from "react";
import { Failure } from "@/components/discussion/failure";
import { useSubmission } from "@/components/discussion/hooks";
import { useMutation } from "@/components/discussion/mutations";
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
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { short } from "@/lib/format";
import type { PullRequest } from "./model";
import { branch as branchName } from "./presentation";

type Action = {
  action: "delete" | "restore";
  version: number;
  repository_id: string;
  head_oid: string;
  retarget?: { base_ref: string; pulls: number[]; token: string };
};

export function HeadBranch({
  repo,
  pull,
  csrf,
  onChanged,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  onChanged: () => void;
}) {
  const source = pull.head_repository ?? repo;
  const upstream = pull.head_repository ? repo.id : undefined;
  const mutation = useMutation(csrf, source);
  const submission = useSubmission();
  const [confirm, setConfirm] = useState<Action>();
  const [saved, setSaved] = useState<{
    branch: string;
    commit: string;
    action: Action["action"];
  }>();
  const route = `pulls/${pull.number}/head-branch`;
  const state = useRequest<{
    branch: Action | null;
    cleanup_pending?: boolean;
  }>(
    endpoint(source, route, { upstream, version: String(pull.version) }),
    source.id,
  );
  const branch = state.data?.branch;
  const name = branchName(pull.head_ref);
  const available = branch?.version === pull.version ? branch : undefined;
  const cleanupPending = state.data?.cleanup_pending;
  const retry = state.retry;
  useEffect(() => {
    if (!cleanupPending) return;
    const timer = window.setTimeout(retry, 2500);
    return () => window.clearTimeout(timer);
  }, [cleanupPending, retry]);

  async function submit(input: Action) {
    const { retarget, ...branch } = input;
    const result = await mutation.run<{ branch: string; commit: string }>(
      endpoint(source, route, { upstream }),
      "POST",
      {
        ...branch,
        request_id: submission(input),
        ...(retarget ? { retarget_token: retarget.token } : {}),
      },
    );
    if (result) {
      setSaved({ ...result, action: input.action });
      setConfirm(undefined);
      state.retry();
      onChanged();
    }
  }

  if (!available && !confirm && !saved) return null;

  return (
    <div className="mt-3 space-y-2">
      {cleanupPending && (
        <p className="text-sm text-muted-foreground" role="status">
          Automatic deletion of <code>{name}</code> is pending.
        </p>
      )}
      {saved && (
        <p className="text-sm" role="status">
          {saved.action === "delete" ? "Deleted" : "Restored"}{" "}
          <code>{branchName(saved.branch)}</code>
          {saved.action === "restore" && (
            <>
              {" "}
              at <code>{short(saved.commit)}</code>
            </>
          )}
          .
        </p>
      )}
      {available?.action === "restore" && (
        <div className="flex flex-wrap items-center gap-3">
          <Button
            type="button"
            variant="outline"
            disabled={mutation.pending}
            onClick={() => void submit(available)}
          >
            {mutation.pending ? "Restoring…" : "Restore branch"}
          </Button>
          <p className="text-sm">
            Restore <code>{name}</code> at{" "}
            <code>{short(available.head_oid)}</code>.
          </p>
        </div>
      )}
      {!confirm && <Failure message={mutation.error} />}
      <Dialog
        open={!!confirm}
        onOpenChange={(open) => {
          if (!open && !mutation.pending) setConfirm(undefined);
        }}
      >
        {available?.action === "delete" && (
          <DialogTrigger asChild>
            <Button
              type="button"
              variant="outline"
              disabled={mutation.pending}
              onClick={() => {
                mutation.reset();
                setConfirm(available);
              }}
            >
              Delete branch
            </Button>
          </DialogTrigger>
        )}
        {confirm && (
          <DialogContent showCloseButton={false}>
            <DialogHeader>
              <DialogTitle>Delete branch?</DialogTitle>
              <DialogDescription>
                Delete <code>{name}</code> at{" "}
                <code>{short(confirm.head_oid)}</code>.
                {confirm.retarget && (
                  <>
                    {" "}
                    Pull{" "}
                    {confirm.retarget.pulls.length === 1
                      ? "request"
                      : "requests"}{" "}
                    {confirm.retarget.pulls
                      .map((pull) => `#${pull}`)
                      .join(", ")}{" "}
                    will move to{" "}
                    <code>{branchName(confirm.retarget.base_ref)}</code>.
                  </>
                )}{" "}
                You can restore this branch while its Git objects are retained.
              </DialogDescription>
            </DialogHeader>
            <Failure message={mutation.error} />
            <DialogFooter>
              <Button
                type="button"
                variant="outline"
                disabled={mutation.pending}
                onClick={() => setConfirm(undefined)}
              >
                Cancel
              </Button>
              <Button
                type="button"
                variant="destructive"
                disabled={mutation.pending}
                onClick={() => void submit(confirm)}
              >
                {mutation.pending ? "Deleting…" : "Delete branch"}
              </Button>
            </DialogFooter>
          </DialogContent>
        )}
      </Dialog>
    </div>
  );
}
