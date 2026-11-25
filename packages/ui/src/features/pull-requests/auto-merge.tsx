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
  can_wait_for_queue?: boolean;
  queue_method?: MergeMethod;
  can_disable: boolean;
  request: {
    request_id: string;
    method: MergeMethod;
    head_oid: string;
    scheduled_by: string;
    stopped: keyof typeof stoppedReasons | null;
    queue?: boolean;
    waiting_for_queue?: boolean;
    first?: boolean;
    testing?: boolean;
  } | null;
}

type Mode = "auto" | "queue" | "when_ready";

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
    mode: Mode;
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
      (confirmation.mode !== "auto"
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

  const refreshState =
    scheduled &&
    (scheduled.stopped === "merged" ||
      ((scheduled.queue || scheduled.waiting_for_queue) && scheduled.stopped) ||
      (scheduled.queue && !pull.merge_queued) ||
      (scheduled.waiting_for_queue && !pull.merge_queue_requested))
      ? scheduled.request_id +
        ":" +
        (scheduled.stopped ?? (scheduled.queue ? "queued" : "waiting"))
      : undefined;
  useEffect(() => {
    if (refreshState && refreshed.current !== refreshState) {
      refreshed.current = refreshState;
      refresh();
    }
  }, [refresh, refreshState]);

  async function change(action: "PUT" | "DELETE", input: object) {
    setMissing(false);
    const result = await save.run<Status | null>(url, action, input);
    setMissing(result === null);
    if (result) {
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
  const canWait =
    !!value?.can_wait_for_queue &&
    pull.branches_available &&
    !pull.merge_requirements.updates_restricted;
  if (!value || (!scheduled && !canSchedule && !canEnqueue && !canWait))
    return null;

  function confirm(mode: Mode, button: HTMLButtonElement) {
    trigger.current = button;
    save.reset();
    setMissing(false);
    setMethod(pull.merge_methods[0] ?? "fast_forward");
    setMessage("");
    setConfirmation({
      version: pull.version,
      base_oid: pull.base_oid,
      head_oid: pull.head_oid,
      mode,
      queueMethod,
      intent: crypto.randomUUID(),
    });
  }
  const whenReady = confirmation?.mode === "when_ready";
  const enqueue = confirmation && confirmation.mode !== "auto";
  const recovering =
    missing ||
    (!!save.error && (save.status === undefined || save.status >= 500));

  return (
    <section
      aria-label={
        scheduled?.queue || scheduled?.waiting_for_queue
          ? "Merge queue"
          : "Auto-merge"
      }
      className="merge-footer grid gap-2"
    >
      {scheduled &&
        (active ? (
          <div className="flex flex-wrap items-center justify-between gap-2">
            <p aria-live="polite">
              <ClockIcon className="mr-1.5 inline" />{" "}
              <strong>
                {scheduled.waiting_for_queue
                  ? "Waiting for merge requirements"
                  : scheduled.queue
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
              {scheduled.queue || scheduled.waiting_for_queue
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
                {scheduled.waiting_for_queue
                  ? "Cancel merge when ready"
                  : scheduled.queue
                    ? "Remove from merge queue"
                    : "Disable auto-merge"}
              </Button>
            )}
          </div>
        ) : (
          scheduled.stopped && (
            <p role="status">
              {scheduled.waiting_for_queue && scheduled.stopped === "canceled"
                ? "Merge when ready canceled."
                : scheduled.queue && scheduled.stopped === "canceled"
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
      {(canSchedule || canEnqueue || canWait) && (
        <div className="flex flex-wrap gap-2">
          {canSchedule && (
            <Button
              variant="outline"
              size="sm"
              onClick={(event) => confirm("auto", event.currentTarget)}
            >
              Enable auto-merge
            </Button>
          )}
          {canEnqueue && (
            <Button
              variant="outline"
              size="sm"
              onClick={(event) => confirm("queue", event.currentTarget)}
            >
              Add to merge queue
            </Button>
          )}
          {canWait && (
            <Button
              variant="outline"
              size="sm"
              onClick={(event) => confirm("when_ready", event.currentTarget)}
            >
              Merge when ready
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
              {whenReady
                ? "Merge when ready"
                : enqueue
                  ? "Add to merge queue"
                  : "Enable auto-merge"}
            </DialogTitle>
            <DialogDescription>
              {whenReady ? (
                <>
                  Once the current reviews and checks pass, Crab will add head{" "}
                  <code>{short(confirmation.head_oid)}</code> to the merge queue
                  and test it against the latest base.
                </>
              ) : enqueue ? (
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
                  when the current merge requirements pass. Auto-merge follows
                  verified updates from people with Write access and checks each
                  new head against the current requirements.
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
                const { intent, mode, queueMethod, ...comparison } =
                  confirmation;
                const input =
                  mode !== "auto"
                    ? {
                        ...comparison,
                        queue: true,
                        method: queueMethod,
                        ...(whenReady ? { when_ready: true } : {}),
                      }
                    : { ...comparison, method, message };
                void change("PUT", {
                  ...input,
                  request_id: submission({ intent, ...input }),
                });
              }}
            >
              {save.pending
                ? enqueue
                  ? whenReady
                    ? "Scheduling…"
                    : "Adding…"
                  : "Enabling…"
                : enqueue
                  ? whenReady
                    ? "Confirm merge when ready"
                    : "Confirm merge queue entry"
                  : "Confirm auto-merge"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </section>
  );
}
