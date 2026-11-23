import { useState } from "react";
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

type DiscussionAddress = {
  repository_id: string;
  subject: { kind: "issue" | "pull"; number: number };
};
export type DiscussionSelection = DiscussionAddress & { version: number };

export function discussionKey({ repository_id, subject }: DiscussionAddress) {
  return `${repository_id}:${subject.kind}:${subject.number}`;
}

type Outcome = DiscussionAddress & {
  status: "unsubscribed" | "changed" | "unavailable" | "unconfirmed";
};
type Progress = { unsubscribed: number; changed: number; unavailable: number };

export function BulkSubscriptions({
  csrf,
  selection,
  onFinished,
}: {
  csrf: string;
  selection: DiscussionSelection[];
  onFinished: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [remaining, setRemaining] = useState<DiscussionSelection[]>([]);
  const [progress, setProgress] = useState<Progress>();
  const mutation = useMutation(csrf, null);

  async function apply() {
    if (!remaining.length || mutation.pending) return;
    const reply = await mutation.run<{ items: Outcome[] }>(
      "/api/notifications/subscriptions/unsubscribe",
      "POST",
      { items: remaining },
    );
    const totals = progress ?? { unsubscribed: 0, changed: 0, unavailable: 0 };
    if (!reply) {
      setProgress(totals);
      return;
    }
    const outcomes = new Map(
      reply.items.map((item) => [discussionKey(item), item.status]),
    );
    const next = { ...totals };
    setRemaining(
      remaining.filter((item) => {
        const status = outcomes.get(discussionKey(item));
        if (!status || status === "unconfirmed") return true;
        next[status] += 1;
        return false;
      }),
    );
    setProgress(next);
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (mutation.pending) return;
        setOpen(next);
        if (next) {
          setRemaining(selection);
          setProgress(undefined);
          mutation.reset();
        }
      }}
    >
      <DialogTrigger asChild>
        <Button size="sm" variant="outline" disabled={!selection.length}>
          Unsubscribe selected
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
          <DialogTitle>Unsubscribe from discussions</DialogTitle>
          <DialogDescription>
            Stop following the selected discussions. Past notifications and
            saved bookmarks remain in your inbox.
          </DialogDescription>
        </DialogHeader>
        {!progress && <p>{remaining.length} selected.</p>}
        {progress && (
          <div className="grid gap-1" role="status">
            <p>
              Unsubscribed from {progress.unsubscribed}{" "}
              {progress.unsubscribed === 1 ? "discussion" : "discussions"}.
            </p>
            {progress.changed > 0 && (
              <p>
                Kept {progress.changed} newer{" "}
                {progress.changed === 1 ? "subscription" : "subscriptions"}.
              </p>
            )}
            {progress.unavailable > 0 && (
              <p>
                {progress.unavailable}{" "}
                {progress.unavailable === 1
                  ? "discussion is"
                  : "discussions are"}{" "}
                unavailable.
              </p>
            )}
            {remaining.length > 0 && (
              <p>
                {remaining.length}{" "}
                {remaining.length === 1 ? "result is" : "results are"}{" "}
                unconfirmed.
              </p>
            )}
          </div>
        )}
        {mutation.error && <p role="alert">{mutation.error}</p>}
        <div className="flex flex-wrap justify-end gap-2">
          <DialogClose asChild>
            <Button variant="outline" disabled={mutation.pending}>
              {progress ? "Close" : "Cancel"}
            </Button>
          </DialogClose>
          {remaining.length > 0 && (
            <Button
              variant="destructive"
              disabled={mutation.pending}
              onClick={() => void apply()}
            >
              {mutation.pending
                ? "Unsubscribing…"
                : progress
                  ? "Retry unconfirmed"
                  : "Unsubscribe"}
            </Button>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
