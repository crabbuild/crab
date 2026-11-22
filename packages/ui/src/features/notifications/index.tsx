import { useEffect, useRef, useState } from "react";
import {
  AtSignIcon,
  BellIcon,
  GitPullRequestIcon,
  UserRoundIcon,
} from "lucide-react";
import { useMutation } from "@/components/discussion/mutations";
import { RelativeTime } from "@/components/discussion/time";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { navigate, useRequest } from "@/lib/api";

type Address = { id: string; owner: string; name: string };

type InboxPage = { items: Notice[]; next: string | null };

function titleOf(repo: Pick<Address, "owner" | "name">) {
  return `${repo.owner}/${repo.name}`;
}

const reasons = {
  subscribed: {
    label: "Watched updates",
    message: "updated a watched discussion",
    icon: BellIcon,
  },
  assigned: {
    label: "Assignments",
    message: "assigned you",
    icon: UserRoundIcon,
  },
  review_requested: {
    label: "Review requests",
    message: "requested your review",
    icon: GitPullRequestIcon,
  },
  mention: { label: "Mentions", message: "mentioned you", icon: AtSignIcon },
  team_mention: {
    label: "Team mentions",
    message: "mentioned your team",
    icon: AtSignIcon,
  },
};

type Reason = keyof typeof reasons;

function isReason(value: string | null): value is Reason {
  return value !== null && Object.hasOwn(reasons, value);
}

type Notice = {
  id: string;
  repository: Pick<Address, "owner" | "name">;
  title: string;
  href: string;
  sender: string;
  reason: Reason;
  read: boolean;
  created_at_ms: number;
};

const INBOX_CHANGED = "crab-inbox-changed";

export function refreshNotificationBadge() {
  window.dispatchEvent(new Event(INBOX_CHANGED));
}

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
  const label = unread ? "Notifications, unread" : "Notifications";
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

function inboxHref(unread: boolean, repository: string, reason: Reason | "") {
  const query = new URLSearchParams();
  if (!unread) query.set("tab", "all");
  if (repository) query.set("repository", repository);
  if (reason) query.set("reason", reason);
  return `/-/notifications${query.size ? `?${query}` : ""}`;
}

export function Notifications({
  csrf,
  url,
  repositories,
}: {
  csrf: string;
  url: URL;
  repositories: Address[];
}) {
  const unread = url.searchParams.get("tab") !== "all";
  const repository = url.searchParams.get("repository") ?? "";
  const selectedReason = url.searchParams.get("reason");
  const reason = isReason(selectedReason) ? selectedReason : "";
  return (
    <section
      className="mx-auto grid max-w-4xl gap-5 px-4 py-6 sm:px-8"
      aria-labelledby="inbox-title"
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h1 id="inbox-title" className="text-2xl font-semibold tracking-tight">
          Notifications
        </h1>
        <nav className="flex gap-1" aria-label="Notification views">
          {[true, false].map((isUnread) => (
            <Button
              key={String(isUnread)}
              asChild
              variant={unread === isUnread ? "secondary" : "ghost"}
            >
              <Link
                href={inboxHref(isUnread, repository, reason)}
                aria-current={unread === isUnread ? "page" : undefined}
              >
                {isUnread ? "Unread" : "All"}
              </Link>
            </Button>
          ))}
        </nav>
      </div>
      {repository && (
        <Link className="w-fit underline" href={inboxHref(unread, "", reason)}>
          Clear repository filter
        </Link>
      )}
      <NotificationList
        key={inboxHref(unread, repository, reason)}
        csrf={csrf}
        unread={unread}
        repository={repository}
        reason={reason}
        repositories={repositories}
      />
    </section>
  );
}

function NotificationList({
  csrf,
  unread,
  repository,
  reason,
  repositories,
}: {
  csrf: string;
  unread: boolean;
  repository: string;
  reason: Reason | "";
  repositories: Address[];
}) {
  const [cursor, setCursor] = useState<string>();
  const [older, setOlder] = useState<Notice[]>([]);
  const [marked, setMarked] = useState<Record<string, boolean>>({});
  const [announcement, setAnnouncement] = useState("");
  const filters = new URLSearchParams({ unread: String(unread) });
  if (repository) filters.set("repository", repository);
  if (reason) filters.set("reason", reason);
  const initial = useRequest<InboxPage>(`/api/notifications?${filters}`);
  if (cursor) filters.set("before", cursor);
  const page = useRequest<InboxPage>(
    cursor ? `/api/notifications?${filters}` : null,
  );
  const repositoryFilter = useRef<HTMLSelectElement>(null);
  return (
    <Result state={initial} timingLabel="Notification inbox">
      {(first) => {
        const items = [...first.items, ...older, ...(page.data?.items ?? [])];
        const next = (cursor ? page.data : first)?.next;
        const visible = items.filter(
          (item) => !unread || !(marked[item.id] ?? item.read),
        );
        return (
          <div className="grid gap-4">
            <div className="flex flex-wrap items-center gap-2">
              <label htmlFor="inbox-repository" className="text-sm font-medium">
                Repository
              </label>
              <NativeSelect
                ref={repositoryFilter}
                id="inbox-repository"
                value={repository}
                onChange={(event) =>
                  navigate(inboxHref(unread, event.target.value, reason))
                }
              >
                <NativeSelectOption value="">
                  All repositories
                </NativeSelectOption>
                {repositories.map((repo) => (
                  <NativeSelectOption key={repo.id} value={repo.id}>
                    {titleOf(repo)}
                  </NativeSelectOption>
                ))}
              </NativeSelect>
              <label htmlFor="inbox-reason" className="text-sm font-medium">
                Reason
              </label>
              <NativeSelect
                id="inbox-reason"
                value={reason}
                onChange={(event) => {
                  const value = event.target.value;
                  navigate(
                    inboxHref(unread, repository, isReason(value) ? value : ""),
                  );
                }}
              >
                <NativeSelectOption value="">All reasons</NativeSelectOption>
                {Object.entries(reasons).map(([value, { label }]) => (
                  <NativeSelectOption key={value} value={value}>
                    {label}
                  </NativeSelectOption>
                ))}
              </NativeSelect>
            </div>
            <p role="status" className="sr-only">
              {announcement}
            </p>
            {visible.length ? (
              <ul
                aria-label="Notifications"
                className="divide-y overflow-hidden rounded-lg border"
              >
                {visible.map((item) => (
                  <NotificationRow
                    key={item.id}
                    item={item}
                    read={marked[item.id] ?? item.read}
                    csrf={csrf}
                    onMarked={(read) => {
                      if (unread && read) repositoryFilter.current?.focus();
                      setAnnouncement(
                        read ? "Marked as read" : "Marked as unread",
                      );
                      setMarked((current) => ({
                        ...current,
                        [item.id]: read,
                      }));
                    }}
                  />
                ))}
              </ul>
            ) : (
              <p className="rounded-lg border px-4 py-8 text-center text-muted-foreground">
                {unread ? "No unread notifications." : "No notifications yet."}
              </p>
            )}
            {page.error && (
              <div className="notice error" role="alert">
                <p>{page.error}</p>
                <Button variant="outline" onClick={page.retry}>
                  Retry loading older notifications
                </Button>
              </div>
            )}
            {page.loading && <p role="status">Loading older notifications…</p>}
            {next && (
              <Button
                className="w-fit"
                variant="outline"
                onClick={() => {
                  if (page.data) setOlder([...older, ...page.data.items]);
                  setCursor(next);
                }}
              >
                Load older notifications
              </Button>
            )}
          </div>
        );
      }}
    </Result>
  );
}

function NotificationRow({
  item,
  read,
  csrf,
  onMarked,
}: {
  item: Notice;
  read: boolean;
  csrf: string;
  onMarked: (read: boolean) => void;
}) {
  const mutation = useMutation(csrf, null);
  const { icon: Icon, message } = reasons[item.reason];
  return (
    <li
      className={`grid grid-cols-[auto_1fr] items-start gap-3 px-3 py-4 sm:grid-cols-[auto_1fr_auto] sm:px-4${read ? "" : " bg-muted/50"}`}
    >
      <Icon aria-hidden="true" className="mt-1 size-4 text-muted-foreground" />
      <div className="min-w-0 space-y-1 text-sm">
        <p className="break-words text-muted-foreground">
          {titleOf(item.repository)}
        </p>
        <Link
          href={item.href}
          className="break-words font-semibold hover:underline"
        >
          {item.title}
        </Link>
        <p className="text-muted-foreground">
          {item.sender} {message} <span aria-hidden="true">·</span>{" "}
          <RelativeTime value={item.created_at_ms} />
        </p>
        {mutation.error && <p role="alert">{mutation.error}</p>}
      </div>
      <Button
        className="col-start-2 w-fit sm:col-start-3"
        variant="outline"
        size="sm"
        disabled={mutation.pending}
        onClick={async () => {
          const result = await mutation.run<null>(
            `/api/notifications/${item.id}/read`,
            "PUT",
            { created_at_ms: item.created_at_ms, read: !read },
          );
          if (result !== undefined) {
            onMarked(!read);
            refreshNotificationBadge();
          }
        }}
      >
        {read ? "Mark as unread" : "Mark as read"}
      </Button>
    </li>
  );
}
