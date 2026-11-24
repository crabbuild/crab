import {
  createContext,
  useContext,
  useEffect,
  useState,
  type ReactNode,
} from "react";
import { LockIcon } from "lucide-react";
import { endpoint, useRequest, type Repository } from "@/lib/api";
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
import { Failure } from "../failure";
import { useMutation } from "../mutations";
import { timestamp } from "../time";

const reasons = {
  off_topic: "Off topic",
  too_heated: "Too heated",
  resolved: "Resolved",
  spam: "Spam",
} as const;

export type LockReason = keyof typeof reasons;
type Duration = "hour" | "day" | "week";
type Lock = {
  version: number;
  locked: boolean;
  reason: LockReason | null;
  can_manage: boolean;
  can_post: boolean;
  expires_at_ms?: number;
  expires_in_ms?: number;
};
type Change =
  | { kind: "lock"; reason: LockReason | null; duration?: Duration }
  | { kind: "unlock" };

const ConversationLock = createContext<{
  lock?: Lock;
  refresh: () => void;
  accept: (lock: Lock) => void;
} | null>(null);

export function useConversationLock() {
  return useContext(ConversationLock);
}

export function ConversationLockProvider({
  repo,
  target,
  children,
}: {
  repo: Repository;
  target: string;
  children: ReactNode;
}) {
  const resource = useRequest<Lock>(endpoint(repo, `${target}/lock`), repo.id);
  const [current, setCurrent] = useState<Lock>();
  useEffect(() => {
    const next = resource.data;
    if (next)
      setCurrent((old) => (old && old.version > next.version ? old : next));
  }, [resource.data]);
  const lock = current ?? resource.data;
  useEffect(() => {
    if (lock?.expires_in_ms === undefined) return;
    // Read the authoritative state again once the reported lock window elapses.
    const timer = setTimeout(resource.retry, Math.max(100, lock.expires_in_ms));
    return () => clearTimeout(timer);
  }, [lock, resource.retry]);
  return (
    <ConversationLock
      value={{
        lock,
        refresh: resource.retry,
        accept: setCurrent,
      }}
    >
      {children}
    </ConversationLock>
  );
}

export function ConversationLockControl({
  repo,
  target,
  csrf,
  onChanged,
}: {
  repo: Repository;
  target: string;
  csrf: string;
  onChanged: () => void;
}) {
  const conversation = useConversationLock();
  const visible = conversation?.lock;
  const mutation = useMutation(csrf, repo);
  const [open, setOpen] = useState(false);
  const [reason, setReason] = useState<LockReason | "">("");
  const [duration, setDuration] = useState<Duration | "">("");
  async function change(change: Change) {
    if (!visible) return;
    const next = await mutation.run<Lock>(
      endpoint(repo, `${target}/lock`),
      "PUT",
      {
        version: visible.version,
        change,
      },
    );
    if (next) {
      conversation?.accept(next);
      setOpen(false);
      setReason("");
      setDuration("");
      conversation?.refresh();
      onChanged();
    }
  }

  if (!visible?.locked && !visible?.can_manage) return null;
  return (
    <div className="mb-3 flex flex-wrap items-center gap-2 text-sm">
      {visible.locked && (
        <span className="flex items-center gap-2" role="status">
          <LockIcon className="size-4" aria-hidden="true" />
          Conversation locked
          {visible.reason ? ` · ${reasons[visible.reason]}` : ""}
          {visible.expires_at_ms !== undefined && (
            <time dateTime={new Date(visible.expires_at_ms).toISOString()}>
              · Expires {timestamp(visible.expires_at_ms)}
            </time>
          )}
        </span>
      )}
      {visible.can_manage &&
        (visible.locked ? (
          <Button
            variant="outline"
            size="sm"
            disabled={mutation.pending || mutation.conflict}
            onClick={() => void change({ kind: "unlock" })}
          >
            Unlock conversation
          </Button>
        ) : (
          <Dialog open={open} onOpenChange={setOpen}>
            <DialogTrigger asChild>
              <Button variant="outline" size="sm">
                Lock conversation
              </Button>
            </DialogTrigger>
            <DialogContent>
              <DialogHeader>
                <DialogTitle>Lock conversation</DialogTitle>
                <DialogDescription>
                  Only people with permission to lock this conversation can post
                  while it is locked.
                </DialogDescription>
              </DialogHeader>
              <form
                className="grid gap-4"
                onSubmit={(event) => {
                  event.preventDefault();
                  void change({
                    kind: "lock",
                    reason: reason || null,
                    duration: duration || undefined,
                  });
                }}
              >
                <label className="grid gap-1 text-sm">
                  Reason
                  <select
                    className="rounded-md border bg-background px-3 py-2"
                    value={reason}
                    onChange={(event) =>
                      setReason(event.currentTarget.value as LockReason | "")
                    }
                  >
                    <option value="">No reason</option>
                    {Object.entries(reasons).map(([key, label]) => (
                      <option key={key} value={key}>
                        {label}
                      </option>
                    ))}
                  </select>
                </label>
                <label className="grid gap-1 text-sm">
                  Duration
                  <select
                    className="rounded-md border bg-background px-3 py-2"
                    value={duration}
                    onChange={(event) =>
                      setDuration(event.currentTarget.value as Duration | "")
                    }
                  >
                    <option value="">Until unlocked</option>
                    <option value="hour">1 hour</option>
                    <option value="day">1 day</option>
                    <option value="week">1 week</option>
                  </select>
                </label>
                <Failure message={mutation.error} />
                <DialogFooter>
                  <Button
                    type="submit"
                    disabled={mutation.pending || mutation.conflict}
                  >
                    Lock
                  </Button>
                </DialogFooter>
              </form>
            </DialogContent>
          </Dialog>
        ))}
      {!open && <Failure message={mutation.error} />}
      {mutation.conflict && (
        <Button
          variant="outline"
          size="sm"
          onClick={() => {
            setOpen(false);
            conversation?.refresh();
            mutation.reset();
          }}
        >
          Reload lock status
        </Button>
      )}
    </div>
  );
}
