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
    summary: "Watched discussion",
    icon: BellIcon,
  },
  assigned: {
    label: "Assignments",
    summary: "Assignment",
    icon: UserRoundIcon,
  },
  review_requested: {
    label: "Review requests",
    summary: "Review request",
    icon: GitPullRequestIcon,
  },
  mention: { label: "Mentions", summary: "Mention", icon: AtSignIcon },
  team_mention: {
    label: "Team mentions",
    summary: "Team mention",
    icon: AtSignIcon,
  },
};

type Reason = keyof typeof reasons;

function isReason(value: string | null): value is Reason {
  return value !== null && Object.hasOwn(reasons, value);
}

type NoticeState = { read: boolean; saved: boolean; done: boolean };
type Action = { read: boolean } | { save: boolean } | { done: boolean };

type Notice = NoticeState & {
  id: string;
  repository: Address;
  title: string;
  href: string;
  sender: string;
  reason: Reason;
  updated: { id: string; created_at_ms: number };
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

const views = {
  unread: { label: "Unread", empty: "No unread notifications." },
  all: { label: "All", empty: "No notifications yet." },
  saved: { label: "Saved", empty: "No saved notifications." },
  done: { label: "Done", empty: "No completed notifications." },
} as const;

type View = keyof typeof views;

function isView(value: string | null): value is View {
  return value !== null && Object.hasOwn(views, value);
}

function inboxHref(view: View, repository: string, reason: Reason | "") {
  const query = new URLSearchParams();
  if (view !== "unread") query.set("tab", view);
  if (repository) query.set("repository", repository);
  if (reason) query.set("reason", reason);
  return `/-/notifications${query.size ? `?${query}` : ""}`;
}

function visibleIn(view: View, state: NoticeState) {
  if (view === "saved") return state.saved;
  if (view === "done") return state.done;
  return !state.done && (view === "all" || !state.read);
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
  const tab = url.searchParams.get("tab");
  const view = isView(tab) ? tab : "unread";
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
        <nav className="flex flex-wrap gap-1" aria-label="Notification views">
          {Object.keys(views)
            .filter(isView)
            .map((item) => (
              <Button
                key={item}
                asChild
                variant={view === item ? "secondary" : "ghost"}
              >
                <Link
                  href={inboxHref(item, repository, reason)}
                  aria-current={view === item ? "page" : undefined}
                >
                  {views[item].label}
                </Link>
              </Button>
            ))}
        </nav>
      </div>
      {repository && (
        <Link className="w-fit underline" href={inboxHref(view, "", reason)}>
          Clear repository filter
        </Link>
      )}
      <NotificationList
        key={inboxHref(view, repository, reason)}
        csrf={csrf}
        view={view}
        repository={repository}
        reason={reason}
        repositories={repositories}
      />
    </section>
  );
}

function NotificationList({
  csrf,
  view,
  repository,
  reason,
  repositories,
}: {
  csrf: string;
  view: View;
  repository: string;
  reason: Reason | "";
  repositories: Address[];
}) {
  const [cursor, setCursor] = useState<string>();
  const [older, setOlder] = useState<Notice[]>([]);
  const [confirmed, setConfirmed] = useState<Record<string, NoticeState>>({});
  const [announcement, setAnnouncement] = useState("");
  const filters = new URLSearchParams({ unread: String(view === "unread") });
  if (view === "saved" || view === "done") filters.set("view", view);
  if (repository) filters.set("repository", repository);
  if (reason) filters.set("reason", reason);
  const initial = useRequest<InboxPage>(`/api/notifications?${filters}`);
  if (cursor) filters.set("before", cursor);
  const page = useRequest<InboxPage>(
    cursor ? `/api/notifications?${filters}` : null,
  );
  const repositoryFilter = useRef<HTMLSelectElement>(null);
  const focusAfterRefresh = useRef(false);
  useEffect(() => {
    if (focusAfterRefresh.current && !initial.loading && initial.data) {
      focusAfterRefresh.current = false;
      repositoryFilter.current?.focus();
    }
  }, [initial.data, initial.loading]);
  return (
    <Result state={initial} timingLabel="Notification inbox">
      {(first) => {
        const threads = new Map<string, Notice>();
        for (const item of [
          ...first.items,
          ...older,
          ...(page.data?.items ?? []),
        ]) {
          if (!threads.has(item.id)) threads.set(item.id, item);
        }
        const next = (cursor ? page.data : first)?.next;
        const visible = Array.from(threads.values())
          .map((item) => ({ ...item, ...confirmed[item.updated.id] }))
          .filter((item) => visibleIn(view, item));
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
                  navigate(inboxHref(view, event.target.value, reason))
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
                    inboxHref(view, repository, isReason(value) ? value : ""),
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
                    csrf={csrf}
                    onRefresh={() => {
                      focusAfterRefresh.current = true;
                      setOlder([]);
                      setCursor(undefined);
                      setConfirmed({});
                      initial.retry();
                    }}
                    onChanged={(state, message) => {
                      if (!visibleIn(view, state))
                        repositoryFilter.current?.focus();
                      setAnnouncement(message);
                      setConfirmed((current) => ({
                        ...current,
                        [item.updated.id]: state,
                      }));
                    }}
                  />
                ))}
              </ul>
            ) : (
              <p className="rounded-lg border px-4 py-8 text-center text-muted-foreground">
                {views[view].empty}
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
  csrf,
  onChanged,
  onRefresh,
}: {
  item: Notice;
  csrf: string;
  onChanged: (state: NoticeState, message: string) => void;
  onRefresh: () => void;
}) {
  const mutation = useMutation(csrf, null);
  const { icon: Icon, summary } = reasons[item.reason];

  async function apply(action: Action, message: string) {
    const result = await mutation.run<NoticeState>(
      `/api/notifications/${item.id}/state`,
      "PUT",
      { repository_id: item.repository.id, expected: item.updated, action },
    );
    if (result !== undefined) {
      onChanged(result, message);
      refreshNotificationBadge();
    }
  }

  return (
    <li
      className={`grid grid-cols-[auto_1fr] items-start gap-3 px-3 py-4 sm:grid-cols-[auto_1fr_auto] sm:px-4${item.read ? "" : " bg-muted/50"}`}
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
          {summary} <span aria-hidden="true">·</span> Updated by {item.sender}{" "}
          <span aria-hidden="true">·</span>{" "}
          <RelativeTime value={item.updated.created_at_ms} />
        </p>
        {mutation.error && <p role="alert">{mutation.error}</p>}
      </div>
      <div className="col-start-2 flex flex-wrap gap-2 sm:col-start-3 sm:justify-end">
        {mutation.status === 409 ? (
          <Button variant="outline" size="sm" onClick={onRefresh}>
            Refresh inbox
          </Button>
        ) : (
          <>
            {!item.done && (
              <Button
                variant="outline"
                size="sm"
                disabled={mutation.pending}
                onClick={() =>
                  apply(
                    { read: !item.read },
                    item.read ? "Marked as unread" : "Marked as read",
                  )
                }
              >
                {item.read ? "Mark as unread" : "Mark as read"}
              </Button>
            )}
            <Button
              variant="outline"
              size="sm"
              disabled={mutation.pending}
              onClick={() =>
                apply(
                  { save: !item.saved },
                  item.saved ? "Removed from Saved" : "Saved",
                )
              }
            >
              {item.saved ? "Unsave" : "Save"}
            </Button>
            <Button
              variant="outline"
              size="sm"
              disabled={mutation.pending}
              onClick={() =>
                apply(
                  { done: !item.done },
                  item.done ? "Restored to inbox" : "Marked as done",
                )
              }
            >
              {item.done ? "Move to inbox" : "Mark as done"}
            </Button>
          </>
        )}
      </div>
    </li>
  );
}
