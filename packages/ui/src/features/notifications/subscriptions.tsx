import { FieldSelect } from "@/components/ui/field-select";
import { useRef, useState } from "react";
import { RelativeTime } from "@/components/discussion/time";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Spinner } from "@/components/ui/spinner";
import { navigate, useRequest } from "@/lib/api";
import type { Address } from "./actions";
import { BulkSubscriptions, discussionKey } from "./bulk-subscriptions";
import { refreshNotificationBadge } from "./index";
import {
  DiscussionWatchButton,
  discussionModeLabel,
  type DiscussionPreference,
} from "./preferences";

type Kind = "issue" | "pull";
const reasons = {
  manual: "Manually subscribed",
  participation: "Participation",
  mention: "Mention",
  assignment: "Assignment",
  unknown: "Source unavailable",
};
type Reason = keyof typeof reasons;
type Subscription = {
  repository: Address;
  subject: { kind: Kind; number: number };
  title: string;
  state: "open" | "closed" | "merged";
  preference: DiscussionPreference;
  origin?: { reason: Reason; subscribed_at_ms: number } | null;
  muted: boolean;
};
type Page = { items: Subscription[]; next: string | null };

export function DiscussionSubscriptions({
  csrf,
  repositories,
  url,
}: {
  csrf: string;
  repositories: Address[];
  url: URL;
}) {
  const repository = url.searchParams.get("repository") ?? "";
  const kind = url.searchParams.get("kind");
  const selectedKind = kind === "issue" || kind === "pull" ? kind : "";
  const reason = url.searchParams.get("reason");
  const selectedReason = reason && Object.hasOwn(reasons, reason) ? reason : "";
  const sort = url.searchParams.get("sort");
  const selectedSort = sort === "recent" || sort === "oldest" ? sort : "";
  const filters = new URLSearchParams();
  if (repository) filters.set("repository", repository);
  if (selectedKind) filters.set("kind", selectedKind);
  if (selectedReason) filters.set("reason", selectedReason);
  if (selectedSort) filters.set("sort", selectedSort);

  function filter(field: string, value: string) {
    const query = new URLSearchParams(filters);
    if (value) query.set(field, value);
    else query.delete(field);
    navigate(`/-/notifications/subscriptions${query.size ? `?${query}` : ""}`);
  }
  return (
    <section
      className="mx-auto grid max-w-4xl gap-5 px-4 py-6 sm:px-8"
      aria-labelledby="subscriptions-title"
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h1
          id="subscriptions-title"
          className="text-2xl font-semibold tracking-tight"
        >
          Subscriptions
        </h1>
        <Button asChild variant="outline">
          <Link href="/-/notifications">Back to notifications</Link>
        </Button>
      </div>
      <div className="flex flex-wrap gap-3">
        <label className="grid gap-1 text-sm font-medium">
          Repository
          <FieldSelect
            value={repository}
            onValueChange={(nextValue) => filter("repository", nextValue)}
            options={[
              { value: "", label: "All repositories" },
              ...(repository &&
              !repositories.some(({ id }) => id === repository)
                ? [{ value: repository, label: "Selected repository" }]
                : []),
              ...repositories.map(({ id, owner, name }) => ({
                value: id,
                label: (
                  <>
                    {owner}/{name}
                  </>
                ),
              })),
            ]}
          />
        </label>
        <label className="grid gap-1 text-sm font-medium">
          Discussion type
          <FieldSelect
            value={selectedKind}
            onValueChange={(nextValue) => filter("kind", nextValue)}
            options={[
              { value: "", label: "Issues and pull requests" },
              { value: "issue", label: "Issues" },
              { value: "pull", label: "Pull requests" },
            ]}
          />
        </label>
        <label className="grid gap-1 text-sm font-medium">
          Subscription reason
          <FieldSelect
            value={selectedReason}
            onValueChange={(nextValue) => filter("reason", nextValue)}
            options={[
              { value: "", label: "All reasons" },
              ...Object.entries(reasons).map(([value, label]) => ({
                value: value,
                label: label,
              })),
            ]}
          />
        </label>
        <label className="grid gap-1 text-sm font-medium">
          Sort subscriptions
          <FieldSelect
            value={selectedSort}
            onValueChange={(nextValue) => filter("sort", nextValue)}
            options={[
              { value: "", label: "Repository order" },
              { value: "recent", label: "Most recently subscribed" },
              { value: "oldest", label: "Least recently subscribed" },
            ]}
          />
        </label>
      </div>
      {selectedSort && !selectedReason && (
        <p className="text-sm text-muted-foreground">
          Subscriptions without a recorded date appear last.
        </p>
      )}
      <SubscriptionList
        key={filters.toString()}
        csrf={csrf}
        filters={filters.toString()}
      />
    </section>
  );
}

function rowKey({ repository, subject }: Subscription) {
  return discussionKey({ repository_id: repository.id, subject });
}

function follows({ mode }: DiscussionPreference) {
  return mode === "follow" || mode === "state_changes";
}

function SubscriptionList({
  csrf,
  filters,
}: {
  csrf: string;
  filters: string;
}) {
  const [after, setAfter] = useState<string>();
  const [earlier, setEarlier] = useState<Subscription[]>([]);
  const [saved, setSaved] = useState<Record<string, DiscussionPreference>>({});
  const [announcement, setAnnouncement] = useState("");
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const list = useRef<HTMLDivElement>(null);
  const query = new URLSearchParams(filters);
  if (after) query.set("after", after);
  const page = useRequest<Page>(`/api/notifications/subscriptions?${query}`);
  const rows = [...earlier, ...(page.data?.items ?? [])].filter((item) =>
    follows(saved[rowKey(item)] ?? item.preference),
  );
  const selection = rows
    .filter((item) => selected.has(rowKey(item)))
    .map((item) => ({
      repository_id: item.repository.id,
      subject: item.subject,
      version: (saved[rowKey(item)] ?? item.preference).version,
    }));
  const loaded = rows.slice(0, 50);
  const allSelected =
    loaded.length > 0 && loaded.every((item) => selected.has(rowKey(item)));

  function startOver() {
    setEarlier([]);
    setSaved({});
    setSelected(new Set());
    setAfter(undefined);
    page.retry();
  }

  return (
    <div
      ref={list}
      tabIndex={-1}
      role="group"
      aria-label="Discussion subscriptions"
      className="grid gap-3"
    >
      <p className="sr-only" role="status">
        {announcement}
      </p>
      {rows.length > 0 && (
        <div className="flex flex-wrap items-center gap-3 rounded-lg border px-4 py-2">
          <label className="flex items-center gap-2 text-sm">
            <Checkbox
              checked={
                allSelected ? true : selection.length ? "indeterminate" : false
              }
              onCheckedChange={() =>
                setSelected(new Set(allSelected ? [] : loaded.map(rowKey)))
              }
            />
            {rows.length > 50
              ? "Select first 50 loaded subscriptions"
              : "Select all loaded subscriptions"}
          </label>
          {selection.length > 0 && (
            <span className="text-sm text-muted-foreground">
              {selection.length} selected
            </span>
          )}
          <BulkSubscriptions
            csrf={csrf}
            selection={selection}
            onFinished={() => {
              refreshNotificationBadge();
              startOver();
              list.current?.focus();
            }}
          />
        </div>
      )}
      {rows.length > 0 && (
        <ul className="divide-y overflow-hidden rounded-lg border">
          {rows.map((item) => {
            const {
              repository: repo,
              subject,
              title,
              state,
              muted,
              origin,
            } = item;
            const key = rowKey(item);
            const isIssue = subject.kind === "issue";
            const preference = saved[key] ?? item.preference;
            const number = `#${subject.number}`;
            return (
              <li
                key={key}
                className="flex flex-wrap items-center justify-between gap-3 px-4 py-3"
              >
                <div className="flex min-w-0 items-start gap-3">
                  <Checkbox
                    aria-label={`Select ${repo.owner}/${repo.name} ${isIssue ? "issue" : "pull request"} ${number}`}
                    checked={selected.has(key)}
                    disabled={!selected.has(key) && selection.length >= 50}
                    onCheckedChange={(checked) =>
                      setSelected((current) => {
                        const next = new Set(current);
                        if (checked === true && next.size < 50) next.add(key);
                        if (checked !== true) next.delete(key);
                        return next;
                      })
                    }
                  />
                  <div className="min-w-0">
                    <Link
                      className="break-words font-medium hover:underline"
                      href={`/${encodeURIComponent(repo.owner)}/${encodeURIComponent(repo.name)}/${isIssue ? "issues" : "pull"}/${subject.number}?${new URLSearchParams({ repository_id: repo.id })}`}
                    >
                      {title}
                    </Link>
                    <div className="break-words text-sm text-muted-foreground">
                      {repo.owner}/{repo.name} {number} ·{" "}
                      {isIssue ? "Issue" : "Pull request"} ·{" "}
                      {state === "open"
                        ? "Open"
                        : state === "closed"
                          ? "Closed"
                          : "Merged"}{" "}
                      · {discussionModeLabel(preference.mode)}
                      {muted && " · Repository muted"}
                    </div>
                    <div className="text-sm text-muted-foreground">
                      {reasons[origin?.reason ?? "unknown"]}
                      {origin && (
                        <>
                          {" "}
                          · <RelativeTime value={origin.subscribed_at_ms} />
                        </>
                      )}
                    </div>
                  </div>
                </div>
                <DiscussionWatchButton
                  repo={repo}
                  csrf={csrf}
                  subject={`${isIssue ? "issues" : "pulls"}/${subject.number}`}
                  label={`Notification settings for ${repo.owner}/${repo.name} ${isIssue ? "issue" : "pull request"} ${number}`}
                  onSaved={(updated) => {
                    setSaved((current) => ({ ...current, [key]: updated }));
                    if (!follows(updated)) {
                      setSelected((current) => {
                        const next = new Set(current);
                        next.delete(key);
                        return next;
                      });
                      list.current?.focus();
                    }
                    setAnnouncement(
                      follows(updated)
                        ? `Updated subscription for ${title}.`
                        : `Stopped following ${title}.`,
                    );
                  }}
                />
              </li>
            );
          })}
        </ul>
      )}
      {page.loading && (
        <p className="notice" role="status">
          <Spinner /> Loading subscriptions…
        </p>
      )}
      {page.error && (
        <div role="alert" className="notice error">
          <p>{page.error}</p>
          <Button variant="outline" onClick={page.retry}>
            Try again
          </Button>
          {after && (
            <Button variant="outline" onClick={startOver}>
              Back to first page
            </Button>
          )}
        </div>
      )}
      {page.data && rows.length === 0 && (
        <p className="text-muted-foreground">
          {page.data.next
            ? "No subscriptions found so far."
            : "No discussion subscriptions match this view."}
        </p>
      )}
      {page.data?.next && (
        <Button
          variant="outline"
          className="justify-self-start"
          onClick={() => {
            const batch = page.data;
            if (!batch?.next) return;
            setEarlier((current) => [...current, ...batch.items]);
            setAfter(batch.next);
          }}
        >
          Load more subscriptions
        </Button>
      )}
    </div>
  );
}
