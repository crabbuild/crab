import { FieldSelect } from "@/components/ui/field-select";
import { useEffect, useRef, useState } from "react";
import { BellIcon } from "lucide-react";
import { useMutation } from "@/components/discussion/mutations";
import { RelativeTime } from "@/components/discussion/time";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { navigate, useRequest, type Repository } from "@/lib/api";
import {
  criteriaAt,
  inboxHref,
  inboxQuery,
  isReason,
  kinds,
  reasons,
  views,
  type Criteria,
  type Reason,
  type View,
} from "./filters";
import { SavedFilters } from "./saved-filters";

import {
  bulkActions,
  INBOX_CHANGED,
  isBulkAction,
  notificationIntent,
  refreshNotificationBadge,
  type Action,
  type Address,
  type BulkAction,
  type BulkIntent,
  type NoticeState,
  type Selection,
  type UnsubscribeStatus,
} from "./actions";
import { EmailNotifications } from "./email";
import { IncomingTransfers, transfersHref } from "./incoming-transfers";
import { IncomingTeamRequests, teamRequestsHref } from "./team-requests";
import {
  OrganizationDirectory,
  organizationInvitationsHref,
} from "@/features/organizations/directory";

export { refreshNotificationBadge } from "./actions";

type InboxPage = { items: Notice[]; next: string | null };

function titleOf(repo: Pick<Address, "owner" | "name">) {
  return `${repo.owner}/${repo.name}`;
}

const MAX_SELECTION = 50;

type Notice = NoticeState &
  Selection & {
    title: string;
    href: string;
    sender: string;
    reason: Reason;
  };

export function NotificationLink({ current }: { current: boolean }) {
  type Attention = { unread: boolean; pending_transfers?: boolean };
  const { data, error, retry } = useRequest<Attention>(
    "/api/notifications/unread",
  );
  const [attention, setAttention] = useState<Attention>();
  useEffect(() => {
    if (data) setAttention(data);
    if (error) setAttention(undefined);
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
  const unread = Boolean(attention?.unread);
  const transfers = Boolean(attention?.pending_transfers);
  const label = unread
    ? "Notifications, unread"
    : transfers
      ? "Notifications, pending repository transfers"
      : "Notifications";
  return (
    <Button variant="ghost" size="icon-lg" className="relative" asChild>
      <Link
        href={transfers ? transfersHref() : "/-/notifications"}
        aria-label={label}
        aria-current={current ? "page" : undefined}
        title={label}
      >
        <BellIcon aria-hidden="true" />
        {(unread || transfers) && (
          <span
            className="absolute top-1 right-1 size-2 rounded-full bg-primary"
            aria-hidden="true"
          />
        )}
      </Link>
    </Button>
  );
}

function visibleIn(view: View, state: NoticeState) {
  if (view === "saved") return state.saved;
  if (view === "done") return state.done;
  return !state.done && (view === "all" || state.read === (view === "read"));
}

export function Notifications({
  csrf,
  url,
  repositories,
}: {
  csrf: string;
  url: URL;
  repositories: Repository[];
}) {
  const tab = url.searchParams.get("tab");
  const transfers = tab === "transfers";
  const invitations = tab === "organization-invitations";
  const teamRequests = tab === "team-requests";
  const discussions = !transfers && !invitations && !teamRequests;
  const criteria = criteriaAt(url);
  const { view, repository, kind, organization, author } = criteria;
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
          {(["unread", "all", "saved", "done"] as const).map((item) => (
            <Button
              key={item}
              asChild
              variant={discussions && view === item ? "secondary" : "ghost"}
            >
              <Link
                href={inboxHref({ ...criteria, view: item })}
                aria-current={discussions && view === item ? "page" : undefined}
              >
                {views[item].label}
              </Link>
            </Button>
          ))}
          <Button asChild variant={transfers ? "secondary" : "ghost"}>
            <Link
              href={transfersHref()}
              aria-current={transfers ? "page" : undefined}
            >
              Repository transfers
            </Link>
          </Button>
          <Button asChild variant={invitations ? "secondary" : "ghost"}>
            <Link
              href={organizationInvitationsHref}
              aria-current={invitations ? "page" : undefined}
            >
              Organization invitations
            </Link>
          </Button>
          <Button asChild variant={teamRequests ? "secondary" : "ghost"}>
            <Link
              href={teamRequestsHref()}
              aria-current={teamRequests ? "page" : undefined}
            >
              Team requests
            </Link>
          </Button>
        </nav>
      </div>
      <div className="flex flex-wrap gap-2">
        <Button asChild variant="outline" size="sm">
          <Link href="/-/notifications/subscriptions">Subscriptions</Link>
        </Button>
        <Button asChild variant="outline" size="sm">
          <Link href="/-/notifications/watched">Watched repositories</Link>
        </Button>
        {csrf && <EmailNotifications csrf={csrf} />}
        {csrf && discussions && (
          <SavedFilters
            csrf={csrf}
            criteria={criteria}
            preset={url.searchParams.get("preset")}
            repositories={repositories}
          />
        )}
      </div>
      {transfers && (
        <IncomingTransfers before={url.searchParams.get("before")} />
      )}
      {invitations && <OrganizationDirectory invitations />}
      {teamRequests && (
        <IncomingTeamRequests after={url.searchParams.get("after")} />
      )}
      {discussions && view === "done" && (
        <p className="text-sm text-muted-foreground">
          Done shows the last 90 days. Bookmarks remain in Saved until you
          unsave them.
        </p>
      )}
      {discussions && repository && (
        <Link
          className="w-fit underline"
          href={inboxHref({ ...criteria, repository: undefined })}
        >
          Clear repository filter
        </Link>
      )}
      {discussions && (organization || kind || author || view === "read") && (
        <div className="flex flex-wrap items-center gap-3 text-sm">
          {organization && (
            <Link
              className="underline"
              href={inboxHref({ ...criteria, organization: undefined })}
            >
              Clear organization filter
            </Link>
          )}
          {kind && (
            <Link
              className="underline"
              href={inboxHref({ ...criteria, kind: undefined })}
            >
              Clear type filter ({kinds[kind]})
            </Link>
          )}
          {author && (
            <Link
              className="underline"
              href={inboxHref({ ...criteria, author: undefined })}
            >
              Clear author filter
            </Link>
          )}
          {view === "read" && (
            <Link
              className="underline"
              href={inboxHref({ ...criteria, view: "all" })}
            >
              Clear read filter
            </Link>
          )}
        </div>
      )}
      {discussions && (
        <NotificationList
          key={inboxHref(criteria)}
          csrf={csrf}
          criteria={criteria}
          repositories={repositories}
        />
      )}
    </section>
  );
}

function NotificationList({
  csrf,
  criteria,
  repositories,
}: {
  csrf: string;
  criteria: Criteria;
  repositories: Address[];
}) {
  const { view, repository, reason } = criteria;
  const [cursor, setCursor] = useState<string>();
  const [older, setOlder] = useState<Notice[]>([]);
  const [confirmed, setConfirmed] = useState<Record<string, NoticeState>>({});
  const [announcement, setAnnouncement] = useState("");
  const [dismissed, setDismissed] = useState<Set<string>>(() => new Set());
  const [skipped, setSkipped] = useState(0);
  const [selection, setSelection] = useState<Set<string>>(() => new Set());
  const [rowPending, setRowPending] = useState(0);
  const [bulkAction, setBulkAction] = useState<BulkAction | "">("");
  const [intent, setIntent] = useState<BulkIntent>();
  const bulk = useMutation(csrf, null);
  const canUnsubscribe = view !== "saved" && view !== "done";
  const filters = inboxQuery(criteria);
  const initial = useRequest<InboxPage>(`/api/notifications?${filters}`);
  if (cursor) filters.set("before", cursor);
  const page = useRequest<InboxPage>(
    cursor ? `/api/notifications?${filters}` : null,
  );
  const repositoryFilter = useRef<HTMLButtonElement>(null);
  const focusAfterRefresh = useRef(false);
  useEffect(() => {
    if (focusAfterRefresh.current && !initial.loading && initial.data) {
      focusAfterRefresh.current = false;
      repositoryFilter.current?.focus();
    }
  }, [initial.data, initial.loading]);
  function refresh() {
    focusAfterRefresh.current = true;
    setOlder([]);
    setCursor(undefined);
    setConfirmed({});
    setDismissed(new Set());
    setSkipped(0);
    setSelection(new Set());
    setBulkAction("");
    setIntent(undefined);
    bulk.reset();
    initial.retry();
  }

  async function applyBulk(submission: BulkIntent) {
    setIntent(submission);
    setSkipped(0);
    const stamps = new Map(
      submission.body.items.map(({ id, expected }) => [id, expected.id]),
    );
    if (submission.kind === "unsubscribe") {
      const result = await bulk.run<{
        items: { id: string; status: UnsubscribeStatus }[];
      }>("/api/notifications/unsubscribe", "POST", submission.body);
      if (!result) return;
      const hidden = result.items.flatMap(({ id, status }) => {
        const stamp = stamps.get(id);
        return stamp && status !== "changed" ? [stamp] : [];
      });
      const count = result.items.filter(
        ({ status }) => status === "unsubscribed",
      ).length;
      setDismissed((current) => new Set([...current, ...hidden]));
      setSkipped(result.items.length - count);
      if (count)
        setAnnouncement(
          `Unsubscribed from ${count === 1 ? "one conversation" : `${count} conversations`}.`,
        );
    } else {
      const result = await bulk.run<{
        items: (NoticeState & { id: string })[];
      }>("/api/notifications/bulk", "POST", submission.body);
      if (!result) return;
      const confirmedStates: Record<string, NoticeState> = {};
      for (const { id, ...state } of result.items) {
        const stamp = stamps.get(id);
        if (stamp) confirmedStates[stamp] = state;
      }
      setConfirmed((current) => ({ ...current, ...confirmedStates }));
      setAnnouncement(`Updated ${result.items.length} notifications.`);
    }
    setSelection(new Set());
    setBulkAction("");
    setIntent(undefined);
    repositoryFilter.current?.focus();
    refreshNotificationBadge();
  }

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
          .filter(
            (item) => !dismissed.has(item.updated.id) && visibleIn(view, item),
          );
        const selected = visible.filter((item) =>
          selection.has(item.updated.id),
        );
        const selectionCanUnsubscribe =
          canUnsubscribe &&
          selected.every(
            (item) =>
              item.subject.kind === "issue" || item.subject.kind === "pull",
          );
        const firstRows = visible.slice(0, MAX_SELECTION);
        const allSelected =
          firstRows.length > 0 &&
          firstRows.every((item) => selection.has(item.updated.id));
        const selectionLocked = !!intent || rowPending > 0;
        const reloadRequired = [400, 404, 409, 410].includes(bulk.status ?? 0);
        return (
          <div className="grid gap-4">
            <div className="flex flex-wrap items-center gap-2">
              <label htmlFor="inbox-repository" className="text-sm font-medium">
                Repository
              </label>
              <FieldSelect
                ref={repositoryFilter}
                id="inbox-repository"
                value={repository ?? ""}
                onValueChange={(value) =>
                  navigate(
                    inboxHref({
                      ...criteria,
                      repository: value || undefined,
                    }),
                  )
                }
                options={[
                  { value: "", label: "All repositories" },
                  ...repositories.map((repo) => ({
                    value: repo.id,
                    label: titleOf(repo),
                  })),
                ]}
              />
              <label htmlFor="inbox-reason" className="text-sm font-medium">
                Reason
              </label>
              <FieldSelect
                id="inbox-reason"
                value={reason ?? ""}
                onValueChange={(value) => {
                  navigate(
                    inboxHref({
                      ...criteria,
                      reason: isReason(value) ? value : undefined,
                    }),
                  );
                }}
                options={[
                  { value: "", label: "All reasons" },
                  ...Object.entries(reasons).map(([value, { label }]) => ({
                    value: value,
                    label: label,
                  })),
                ]}
              />
            </div>
            <p role="status" className="sr-only">
              {announcement}
            </p>
            {(bulk.error || skipped > 0) && (
              <div
                className="notice error flex flex-wrap items-center gap-2"
                role="alert"
              >
                <p>
                  {bulk.error ??
                    (skipped === 1
                      ? "One conversation changed or is unavailable."
                      : `${skipped} conversations changed or are unavailable.`)}
                </p>
                {intent && !reloadRequired && (
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => void applyBulk(intent)}
                  >
                    {intent.kind === "unsubscribe"
                      ? "Retry unsubscribe"
                      : "Retry bulk action"}
                  </Button>
                )}
                <Button variant="outline" size="sm" onClick={refresh}>
                  Refresh inbox
                </Button>
              </div>
            )}
            {visible.length ? (
              <>
                <div className="flex flex-wrap items-center gap-3 rounded-lg border px-4 py-2">
                  <Checkbox
                    id="inbox-select-all"
                    aria-label={
                      visible.length > MAX_SELECTION
                        ? `Select first ${MAX_SELECTION} loaded notifications`
                        : "Select all loaded notifications"
                    }
                    checked={
                      allSelected
                        ? true
                        : selected.length
                          ? "indeterminate"
                          : false
                    }
                    disabled={selectionLocked}
                    onCheckedChange={() =>
                      setSelection(
                        new Set(
                          allSelected
                            ? []
                            : firstRows.map((item) => item.updated.id),
                        ),
                      )
                    }
                  />
                  <label htmlFor="inbox-select-all" className="text-sm">
                    {selected.length
                      ? `${selected.length} selected`
                      : "Select notifications"}
                  </label>
                  {!!selected.length && (
                    <div className="flex flex-wrap items-center gap-2">
                      <FieldSelect
                        aria-label="Bulk notification action"
                        value={bulkAction}
                        onValueChange={(nextValue) => {
                          const value = nextValue;
                          setBulkAction(isBulkAction(value) ? value : "");
                        }}
                        disabled={!!intent}
                        options={[
                          { value: "", label: "Choose action" },
                          ...Object.keys(bulkActions)
                            .filter(isBulkAction)
                            .filter((action) =>
                              action === "unsubscribe"
                                ? selectionCanUnsubscribe
                                : view !== "done" ||
                                  !["read", "unread", "done"].includes(action),
                            )
                            .map((action) => ({
                              value: action,
                              label: bulkActions[action].label,
                            })),
                        ]}
                      />
                      <Button
                        variant="outline"
                        size="sm"
                        disabled={
                          !bulkAction ||
                          selectionLocked ||
                          (bulkAction === "unsubscribe" &&
                            !selectionCanUnsubscribe)
                        }
                        onClick={() => {
                          if (!bulkAction) return;
                          void applyBulk(
                            notificationIntent(bulkAction, selected),
                          );
                        }}
                      >
                        Apply to selected
                      </Button>
                    </div>
                  )}
                </div>
                <ul
                  aria-label="Notifications"
                  className="divide-y overflow-hidden rounded-lg border"
                >
                  {visible.map((item) => {
                    const checked = selection.has(item.updated.id);
                    return (
                      <NotificationRow
                        key={item.id}
                        item={item}
                        csrf={csrf}
                        checked={checked}
                        selectionDisabled={
                          selectionLocked ||
                          (!checked && selected.length >= MAX_SELECTION)
                        }
                        actionsDisabled={!!intent || selected.length > 0}
                        onUnsubscribe={
                          canUnsubscribe &&
                          (item.subject.kind === "issue" ||
                            item.subject.kind === "pull")
                            ? () =>
                                void applyBulk(
                                  notificationIntent("unsubscribe", [item]),
                                )
                            : undefined
                        }
                        onSelect={() => {
                          setSelection((current) => {
                            const next = new Set(current);
                            if (checked) next.delete(item.updated.id);
                            else next.add(item.updated.id);
                            return next;
                          });
                        }}
                        onPending={(pending) =>
                          setRowPending(
                            (current) => current + (pending ? 1 : -1),
                          )
                        }
                        onRefresh={refresh}
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
                    );
                  })}
                </ul>
              </>
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
  checked,
  selectionDisabled,
  actionsDisabled,
  onUnsubscribe,
  onSelect,
  onPending,
  onChanged,
  onRefresh,
}: {
  item: Notice;
  csrf: string;
  checked: boolean;
  selectionDisabled: boolean;
  actionsDisabled: boolean;
  onUnsubscribe?: () => void;
  onSelect: () => void;
  onPending: (pending: boolean) => void;
  onChanged: (state: NoticeState, message: string) => void;
  onRefresh: () => void;
}) {
  const mutation = useMutation(csrf, null);
  const { icon: Icon, summary } = reasons[item.reason];

  async function apply(action: Action, message: string) {
    onPending(true);
    try {
      const result = await mutation.run<NoticeState>(
        `/api/notifications/${item.id}/state`,
        "PUT",
        { repository_id: item.repository.id, expected: item.updated, action },
      );
      if (result !== undefined) {
        onChanged(result, message);
        refreshNotificationBadge();
      }
    } finally {
      onPending(false);
    }
  }

  return (
    <li
      className={`grid grid-cols-[auto_auto_1fr] items-start gap-3 px-3 py-4 sm:grid-cols-[auto_auto_1fr_auto] sm:px-4${item.read ? "" : " bg-muted/50"}`}
    >
      <Checkbox
        className="mt-1"
        aria-label={`Select ${item.title} in ${titleOf(item.repository)}`}
        checked={checked}
        disabled={selectionDisabled}
        onCheckedChange={onSelect}
      />
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
      <div className="col-start-3 flex flex-wrap gap-2 sm:col-start-4 sm:justify-end">
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
                disabled={mutation.pending || actionsDisabled}
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
              disabled={mutation.pending || actionsDisabled}
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
              disabled={mutation.pending || actionsDisabled}
              onClick={() =>
                apply(
                  { done: !item.done },
                  item.done ? "Restored to inbox" : "Marked as done",
                )
              }
            >
              {item.done ? "Move to inbox" : "Mark as done"}
            </Button>
            {onUnsubscribe && (
              <Button
                variant="outline"
                size="sm"
                disabled={mutation.pending || actionsDisabled}
                onClick={onUnsubscribe}
              >
                Unsubscribe
              </Button>
            )}
          </>
        )}
      </div>
    </li>
  );
}
