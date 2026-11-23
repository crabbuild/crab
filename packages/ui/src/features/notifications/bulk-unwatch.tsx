import { useEffect, useId, useRef, useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { request } from "@/lib/api";
import type { WatchPage } from "./watched";

type Selection = { id: string; version: number };
type Outcome = {
  id: string;
  status: "unwatched" | "changed" | "unavailable" | "unconfirmed";
};
type Progress = {
  unwatched: number;
  changed: number;
  unavailable: number;
  remaining: Selection[];
};

async function ownerWatches(owner: string, signal: AbortSignal) {
  const rows = new Map<string, Selection>();
  const query = new URLSearchParams({ owner, limit: "50" });
  do {
    const { data } = await request<WatchPage>(
      `/api/notifications/watched?${query}`,
      signal,
    );
    for (const { repository, preference } of data.items)
      rows.set(repository.id, {
        id: repository.id,
        version: preference.version,
      });
    if (!data.next) break;
    query.set("after", data.next);
  } while (!signal.aborted);
  return [...rows.values()];
}

export function BulkUnwatch({
  csrf,
  owners,
  onFinished,
}: {
  csrf: string;
  owners: string[];
  onFinished: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [owner, setOwner] = useState("");
  const [attempt, setAttempt] = useState(0);
  const [preview, setPreview] = useState<
    { owner: string; attempt: number } & (
      | { items: Selection[]; error?: never }
      | { items?: never; error: string }
    )
  >();
  const [progress, setProgress] = useState<Progress>();
  const [running, setRunning] = useState(false);
  const submitting = useRef(false);
  const ownerId = useId();
  const mutation = useMutation(csrf, null);
  useEffect(() => {
    if (!open || !owner) return;
    const controller = new AbortController();
    ownerWatches(owner, controller.signal).then(
      (items) => {
        if (!controller.signal.aborted) setPreview({ owner, attempt, items });
      },
      (error: unknown) => {
        if (!controller.signal.aborted)
          setPreview({
            owner,
            attempt,
            error:
              error instanceof Error ? error.message : "Unable to load watches",
          });
      },
    );
    return () => controller.abort();
  }, [open, owner, attempt]);
  const reviewed =
    preview?.owner === owner && preview.attempt === attempt
      ? preview
      : undefined;
  const pending = progress?.remaining ?? reviewed?.items ?? [];

  async function apply() {
    if (submitting.current || !pending.length) return;
    submitting.current = true;
    setRunning(true);
    const totals = {
      unwatched: progress?.unwatched ?? 0,
      changed: progress?.changed ?? 0,
      unavailable: progress?.unavailable ?? 0,
      remaining: [] as Selection[],
    };
    try {
      for (let offset = 0; offset < pending.length; offset += 50) {
        const items = pending.slice(offset, offset + 50);
        const reply = await mutation.run<{ items: Outcome[] }>(
          "/api/notifications/watched/unwatch",
          "POST",
          { owner, items },
        );
        if (!reply) {
          totals.remaining.push(...pending.slice(offset));
          break;
        }
        const outcomes = new Map(
          reply.items.map(({ id, status }) => [id, status]),
        );
        for (const item of items) {
          const status = outcomes.get(item.id);
          if (!status || status === "unconfirmed") totals.remaining.push(item);
          else totals[status] += 1;
        }
      }
    } finally {
      setProgress(totals);
      submitting.current = false;
      setRunning(false);
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (submitting.current) return;
        setOpen(next);
        if (next) {
          setOwner(owners.length === 1 ? owners[0] : "");
          setPreview(undefined);
          setProgress(undefined);
          mutation.reset();
        }
      }}
    >
      <DialogTrigger asChild>
        <Button variant="outline" disabled={!owners.length}>
          Unwatch all
        </Button>
      </DialogTrigger>
      <DialogContent
        showCloseButton={false}
        onCloseAutoFocus={(event) => {
          if (progress) {
            event.preventDefault();
            onFinished();
          }
        }}
      >
        <DialogHeader>
          <DialogTitle>Unwatch repositories by owner</DialogTitle>
          <DialogDescription>
            Stop receiving repository updates from the reviewed watches. Direct
            notifications and individual discussion subscriptions stay
            available.
          </DialogDescription>
        </DialogHeader>
        <div className="grid gap-2">
          <label htmlFor={ownerId} className="font-medium">
            Repository owner
          </label>
          <NativeSelect
            id={ownerId}
            value={owner}
            disabled={running || !!progress}
            onChange={(event) => setOwner(event.target.value)}
          >
            <NativeSelectOption value="">Choose an owner</NativeSelectOption>
            {owners.map((name) => (
              <NativeSelectOption key={name} value={name}>
                {name}
              </NativeSelectOption>
            ))}
          </NativeSelect>
        </div>
        {owner && !reviewed && (
          <p role="status">Loading watches for {owner}…</p>
        )}
        {reviewed?.error && (
          <div role="alert" className="grid gap-2">
            <p>{reviewed.error}</p>
            <Button
              variant="outline"
              className="w-fit"
              onClick={() => setAttempt((value) => value + 1)}
            >
              Reload watches
            </Button>
          </div>
        )}
        {!progress && reviewed?.items && (
          <p>
            {pending.length
              ? `${pending.length} watched ${pending.length === 1 ? "repository" : "repositories"} owned by ${owner}.`
              : `No active watches for ${owner}.`}
          </p>
        )}
        {running && <p role="status">Stopping repository watches…</p>}
        {progress && (
          <div role="status" className="grid gap-1">
            <p>
              Stopped watching {progress.unwatched}{" "}
              {progress.unwatched === 1 ? "repository" : "repositories"}.
            </p>
            {progress.changed > 0 && (
              <p>
                Kept {progress.changed} newer watch{" "}
                {progress.changed === 1 ? "setting" : "settings"}.
              </p>
            )}
            {progress.unavailable > 0 && (
              <p>
                {progress.unavailable}{" "}
                {progress.unavailable === 1
                  ? "repository is"
                  : "repositories are"}{" "}
                unavailable.
              </p>
            )}
            {pending.length > 0 && (
              <p>{pending.length} results could not be confirmed.</p>
            )}
          </div>
        )}
        {mutation.error && <p role="alert">{mutation.error}</p>}
        <div className="flex flex-wrap justify-end gap-2">
          <DialogClose asChild>
            <Button variant="outline" disabled={running}>
              {progress ? "Close" : "Cancel"}
            </Button>
          </DialogClose>
          {pending.length > 0 && (
            <Button
              variant="destructive"
              disabled={running}
              onClick={() => void apply()}
            >
              {progress
                ? "Retry unconfirmed"
                : `Unwatch ${pending.length} ${pending.length === 1 ? "repository" : "repositories"}`}
            </Button>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
