import { FieldSelect } from "@/components/ui/field-select";
import { useEffect, useRef, useState } from "react";
import { ClockIcon } from "@primer/octicons-react";
import { Link } from "@/components/navigation/link";
import { repoHref } from "@/lib/repository-route";
import { useSubmission } from "@/components/discussion/hooks";
import { Failure } from "@/components/discussion/failure";
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
import { Input } from "@/components/ui/input";
import { endpoint, request, useRequest, type Repository } from "@/lib/api";
import { short } from "@/lib/format";
import { mergeMethod } from "./merge-methods";
import type { MergeMethod, PullRequest } from "./model";

const stoppedReasons = {
  canceled: "Auto-merge was disabled for this pull request.",
  head_changed:
    "The head commit changed. Review it before enabling auto-merge again.",
  pull_changed:
    "The pull request changed. Review it before enabling auto-merge again.",
  permission_changed:
    "The person who enabled auto-merge no longer has Write access.",
  disabled: "Auto-merge was disabled in repository settings.",
  method_disabled: "The selected merge method is disabled.",
  queue_required: "The target branch now requires the merge queue.",
  conflict:
    "Crab could not merge this comparison. Review the branch changes before trying again.",
  checks_failed: "A required check failed on the merge candidate.",
  checks_timed_out: "Required checks did not finish in time.",
  requirements_changed:
    "The pull request no longer satisfies the branch requirements.",
  path_locked: "A changed path is locked by another user.",
  merged: "The pull request merged.",
};

interface Status {
  can_enable: boolean;
  can_enqueue?: boolean;
  queue_method?: MergeMethod;
  can_disable: boolean;
  request: {
    request_id: string;
    method: MergeMethod;
    head_oid: string;
    scheduled_by: string;
    stopped: keyof typeof stoppedReasons | null;
    queue?: boolean;
    first?: boolean;
    testing?: boolean;
  } | null;
}

export function AutoMerge({
  repo,
  pull,
  csrf,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  refresh: () => void;
}) {
  const url = endpoint(repo, `pulls/${pull.number}/auto-merge`);
  const initial = useRequest<Status>(url);
  const [status, setStatus] = useState<Status>();
  const [confirmation, setConfirmation] = useState<{
    version: number;
    base_oid: string;
    head_oid: string;
    queue: boolean;
    queueMethod: MergeMethod;
    intent: string;
  }>();
  const [method, setMethod] = useState(pull.merge_methods[0] ?? "fast_forward");
  const [message, setMessage] = useState("");
  const [missing, setMissing] = useState(false);
  const save = useMutation(csrf, repo);
  const submission = useSubmission();
  const value = status ?? initial.data;
  const queueMethod = value?.queue_method ?? "merge_commit";
  const scheduled = value?.request;
  const active = !!scheduled && scheduled.stopped === null;
  const refreshed = useRef<string | undefined>(undefined);
  const trigger = useRef<HTMLButtonElement | null>(null);
  const stale =
    confirmation &&
    (confirmation.version !== pull.version ||
      confirmation.base_oid !== pull.base_oid ||
      confirmation.head_oid !== pull.head_oid ||
      (confirmation.queue
        ? confirmation.queueMethod !== queueMethod
        : !pull.merge_methods.includes(method)));

  useEffect(() => {
    if (!active || save.pending) return;
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout>;
    async function poll() {
      try {
        const response = await request<Status>(url, controller.signal);
        if (!controller.signal.aborted && response.data)
          setStatus(response.data);
      } catch {
        // Keep the last observed state during a failed refresh.
      } finally {
        if (!controller.signal.aborted)
          timer = setTimeout(() => void poll(), 3000);
      }
    }
    timer = setTimeout(() => void poll(), 3000);
    return () => {
      controller.abort();
      clearTimeout(timer);
    };
  }, [active, save.pending, url]);

  useEffect(() => {
    if (
      (scheduled?.stopped === "merged" ||
        (scheduled?.queue && scheduled.stopped)) &&
      refreshed.current !== scheduled.request_id
    ) {
      refreshed.current = scheduled.request_id;
      refresh();
    }
  }, [refresh, scheduled]);

  async function change(action: "PUT" | "DELETE", input: object) {
    setMissing(false);
    const result = await save.run<Status | null>(url, action, input);
    setMissing(result === null);
    if (result) {
      if (result.request?.queue) refresh();
      setStatus(result);
      setConfirmation(undefined);
    }
  }

  const canSchedule =
    !!value?.can_enable && !pull.can_merge && pull.branches_available;
  const canEnqueue =
    !!value?.can_enqueue &&
    pull.branches_available &&
    !pull.merge_requirements.updates_restricted;
  if (!value || (!scheduled && !canSchedule && !canEnqueue)) return null;

  function confirm(queue: boolean, button: HTMLButtonElement) {
    trigger.current = button;
    save.reset();
    setMissing(false);
    setMethod(pull.merge_methods[0] ?? "fast_forward");
    setMessage("");
    setConfirmation({
      version: pull.version,
      base_oid: pull.base_oid,
      head_oid: pull.head_oid,
      queue,
      queueMethod,
      intent: crypto.randomUUID(),
    });
  }
  const enqueue = confirmation?.queue;
  const recovering =
    missing ||
    (!!save.error && (save.status === undefined || save.status >= 500));

  return (
    <section
      aria-label={scheduled?.queue ? "Merge queue" : "Auto-merge"}
      className="merge-footer grid gap-2"
    >
      {scheduled &&
        (active ? (
          <div className="flex flex-wrap items-center justify-between gap-2">
            <p>
              <ClockIcon className="mr-1.5 inline" />{" "}
              <strong>
                {scheduled.queue
                  ? scheduled.first
                    ? scheduled.testing
                      ? "Testing merge candidate"
                      : "Preparing merge candidate"
                    : scheduled.testing
                      ? "Testing with earlier queue entries"
                      : "Waiting in the merge queue"
                  : "Auto-merge enabled"}
              </strong>{" "}
              ·{" "}
              {scheduled.queue
                ? "Head"
                : mergeMethod(scheduled.method).label + " at"}{" "}
              <code>{short(scheduled.head_oid)}</code> ·{" "}
              {scheduled.scheduled_by}
              {scheduled.queue && (
                <>
                  {" · "}
                  <Link
                    className="underline"
                    href={repoHref(repo, {
                      view: "pulls",
                      action: "queue",
                      branch: pull.base_ref,
                    })}
                  >
                    View merge queue
                  </Link>
                </>
              )}
              {scheduled.queue && scheduled.testing && (
                <>
                  {" · "}
                  <Link
                    className="underline"
                    href={repoHref(repo, {
                      view: "pulls",
                      pull: String(pull.number),
                      pull_tab: "checks",
                      candidate: "1",
                    })}
                  >
                    Candidate checks
                  </Link>
                </>
              )}
            </p>
            {value.can_disable && (
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={save.pending}
                onClick={() =>
                  void change("DELETE", { request_id: scheduled.request_id })
                }
              >
                {scheduled.queue
                  ? "Remove from merge queue"
                  : "Disable auto-merge"}
              </Button>
            )}
          </div>
        ) : (
          scheduled.stopped && (
            <p role="status">
              {scheduled.queue && scheduled.stopped === "canceled"
                ? "Removed from the merge queue."
                : scheduled.queue && scheduled.stopped === "disabled"
                  ? "The merge queue was disabled in repository settings."
                  : scheduled.queue && scheduled.stopped === "method_disabled"
                    ? "The queue merge method changed. Add the pull request again to test it."
                    : stoppedReasons[scheduled.stopped]}
            </p>
          )
        ))}
      {!confirmation && (
        <Failure
          message={
            save.error ??
            (missing
              ? "The reply was lost. Retry to recover the recorded request."
              : undefined)
          }
        />
      )}
      {(canSchedule || canEnqueue) && (
        <div className="flex flex-wrap gap-2">
          {canSchedule && (
            <Button
              variant="outline"
              size="sm"
              onClick={(event) => confirm(false, event.currentTarget)}
            >
              Enable auto-merge
            </Button>
          )}
          {canEnqueue && (
            <Button
              variant="outline"
              size="sm"
              onClick={(event) => confirm(true, event.currentTarget)}
            >
              Add to merge queue
            </Button>
          )}
        </div>
      )}
      <Dialog
        open={!!confirmation}
        onOpenChange={(open) => {
          if (!open && !save.pending) setConfirmation(undefined);
        }}
      >
        <DialogContent
          onCloseAutoFocus={(event) => {
            if (trigger.current?.isConnected) {
              event.preventDefault();
              trigger.current.focus();
            }
          }}
        >
          <DialogHeader>
            <DialogTitle>
              {enqueue ? "Add to merge queue" : "Enable auto-merge"}
            </DialogTitle>
            <DialogDescription>
              {enqueue ? (
                <>
                  Crab will test head{" "}
                  <code>{short(confirmation?.head_oid ?? pull.head_oid)}</code>{" "}
                  against the latest base using{" "}
                  {
                    mergeMethod(confirmation?.queueMethod ?? queueMethod)
                      .pending
                  }
                  .
                </>
              ) : (
                <>
                  Crab will merge head{" "}
                  <code>{short(confirmation?.head_oid ?? pull.head_oid)}</code>{" "}
                  when the current merge requirements pass.
                </>
              )}
            </DialogDescription>
          </DialogHeader>
          {!enqueue && (
            <label className="grid gap-1">
              Merge method
              <FieldSelect
                value={method}
                disabled={save.pending || recovering}
                onValueChange={(nextValue) => {
                  const selected = pull.merge_methods.find(
                    (method) => method === nextValue,
                  );
                  if (selected) setMethod(selected);
                }}
                options={[
                  ...pull.merge_methods.map((method) => ({
                    value: method,
                    label: mergeMethod(method).label,
                  })),
                ]}
              />
            </label>
          )}
          {!enqueue && (method === "merge_commit" || method === "squash") && (
            <label className="grid gap-1">
              Commit message{" "}
              <Input
                value={message}
                onChange={(event) => setMessage(event.target.value)}
                maxLength={256}
                disabled={save.pending || recovering}
                placeholder="Use the default message"
              />
            </label>
          )}
          {stale && (
            <p role="alert">
              The comparison changed. Close this dialog and review the current
              commits.
            </p>
          )}
          <Failure
            message={
              save.error ??
              (missing
                ? "The reply was lost. Retry to recover the recorded request."
                : undefined)
            }
          />
          <DialogFooter>
            {enqueue && save.status === 409 && (
              <Button
                variant="outline"
                onClick={() => {
                  setStatus(undefined);
                  setConfirmation(undefined);
                  initial.retry();
                }}
              >
                Refresh queue settings
              </Button>
            )}
            <Button
              variant="outline"
              disabled={save.pending}
              onClick={() => setConfirmation(undefined)}
            >
              Cancel
            </Button>
            <Button
              disabled={save.pending || stale}
              onClick={() => {
                if (!confirmation) return;
                const { intent, queue, queueMethod, ...comparison } =
                  confirmation;
                const input = queue
                  ? { ...comparison, queue, method: queueMethod }
                  : { ...comparison, method, message };
                void change("PUT", {
                  ...input,
                  request_id: submission({ intent, ...input }),
                });
              }}
            >
              {save.pending
                ? enqueue
                  ? "Adding…"
                  : "Enabling…"
                : enqueue
                  ? "Confirm merge queue entry"
                  : "Confirm auto-merge"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </section>
  );
}
