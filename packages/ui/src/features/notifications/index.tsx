import { useEffect, useState } from "react";
import { BellIcon } from "lucide-react";
import { useMutation } from "@/components/discussion/mutations";
import { RelativeTime } from "@/components/discussion/time";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { useRequest } from "@/lib/api";

type Notice = {
  id: string;
  repository: { owner: string; name: string };
  title: string;
  href: string;
  sender: string;
  reason: "mention" | "team_mention";
  read: boolean;
  created_at_ms: number;
};

const INBOX_CHANGED = "crab-inbox-changed";

export function NotificationLink({ current }: { current: boolean }) {
  const { data, error, retry } = useRequest<{ unread: boolean }>(
    "/api/notifications/unread",
  );
  const [unread, setUnread] = useState(false);
  useEffect(() => {
    if (data) setUnread(data.unread);
    if (error) setUnread(false);
  }, [data, error]);
  useEffect(() => {
    function refresh() {
      if (!document.hidden) retry();
    }
    const timer = window.setInterval(refresh, 30_000);
    document.addEventListener("visibilitychange", refresh);
    window.addEventListener(INBOX_CHANGED, refresh);
    return () => {
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", refresh);
      window.removeEventListener(INBOX_CHANGED, refresh);
    };
  }, [retry]);
  const label = unread ? "Notifications, unread mentions" : "Notifications";
  return (
    <Button variant="ghost" size="icon-lg" className="relative" asChild>
      <Link
        href="/-/notifications"
        aria-label={label}
        aria-current={current ? "page" : undefined}
        title={label}
      >
        <BellIcon aria-hidden="true" />
        {unread && (
          <span
            className="absolute top-1 right-1 size-2 rounded-full bg-primary"
            aria-hidden="true"
          />
        )}
      </Link>
    </Button>
  );
}

export function Notifications({ csrf }: { csrf: string }) {
  const [before, setBefore] = useState<string | null>(null);
  const inbox = useRequest<{ items: Notice[]; next: string | null }>(
    before
      ? `/api/notifications?${new URLSearchParams({ before })}`
      : "/api/notifications",
  );
  function refresh() {
    if (before) setBefore(null);
    else inbox.retry();
    window.dispatchEvent(new Event(INBOX_CHANGED));
  }
  return (
    <section className="mx-auto w-full max-w-4xl px-4 py-6 sm:px-6">
      <div className="mb-4 flex flex-wrap items-center justify-between gap-2">
        <h1 className="text-2xl font-semibold tracking-tight">Notifications</h1>
        <Button variant="outline" disabled={inbox.loading} onClick={refresh}>
          {before ? "Latest" : "Refresh"}
        </Button>
      </div>
      {inbox.loading && (
        <div className="notice" role="status">
          <Spinner /> Loading notifications…
        </div>
      )}
      {inbox.error && (
        <div className="notice error" role="alert">
          <p>{inbox.error}</p>
          <Button variant="outline" onClick={inbox.retry}>
            Try again
          </Button>
        </div>
      )}
      {inbox.data &&
        (inbox.data.items.length ? (
          <ul
            aria-label="Mention notifications"
            className="divide-y rounded-md border"
          >
            {inbox.data.items.map((item) => (
              <Notification key={item.id} item={item} csrf={csrf} />
            ))}
          </ul>
        ) : (
          <p className="notice">No notifications.</p>
        ))}
      {inbox.data?.next && (
        <Button
          className="mt-4"
          variant="outline"
          onClick={() => setBefore(inbox.data?.next ?? null)}
        >
          Older notifications
        </Button>
      )}
    </section>
  );
}

function Notification({ item, csrf }: { item: Notice; csrf: string }) {
  const [marked, setMarked] = useState(false);
  const mutation = useMutation(csrf, null);
  const read = item.read || marked;
  async function markRead() {
    const result = await mutation.run<null>(
      `/api/notifications/${item.id}/read`,
      "PUT",
      { created_at_ms: item.created_at_ms },
    );
    if (result !== undefined) {
      setMarked(true);
      window.dispatchEvent(new Event(INBOX_CHANGED));
    }
  }
  return (
    <li className="flex flex-wrap items-start justify-between gap-x-4 gap-y-2 p-4">
      <div className="min-w-0 grow basis-56 space-y-1">
        <p className="text-xs break-words text-muted-foreground">
          {item.repository.owner}/{item.repository.name} ·{" "}
          <RelativeTime value={item.created_at_ms} />
        </p>
        <Link
          href={item.href}
          className="block w-fit font-medium break-words hover:underline"
        >
          {item.title}
        </Link>
        <p className="text-sm break-words text-muted-foreground">
          {item.sender}{" "}
          {item.reason === "team_mention"
            ? "mentioned your team"
            : "mentioned you"}
        </p>
        {mutation.error && (
          <p className="text-sm text-destructive" role="alert">
            {mutation.error}
          </p>
        )}
      </div>
      {read ? (
        <span className="px-2 text-xs text-muted-foreground" role="status">
          Read
        </span>
      ) : (
        <Button
          variant="outline"
          size="sm"
          disabled={mutation.pending}
          onClick={markRead}
        >
          {mutation.pending ? "Marking as read…" : "Mark as read"}
        </Button>
      )}
    </li>
  );
}
