import { useRef, useState } from "react";
import { AtSignIcon, GitPullRequestIcon } from "lucide-react";
import { Author, type DiscussionAuthor } from "@/components/discussion/author";
import type { Target } from "@/components/discussion/conversation/model";
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
import { repoHref } from "@/lib/repository-route";

interface Address {
  id: string;
  owner: string;
  name: string;
}

interface Notification {
  repository: Address;
  event: number;
  at: number;
  target: Target;
  title: string;
  actor: DiscussionAuthor;
  reason: "review_requested" | "mention";
  read: boolean;
}

interface InboxPage {
  items: Notification[];
  next: string | null;
  repositories: Address[];
  scope_required: boolean;
}

function titleOf(repo: Address) {
  return `${repo.owner}/${repo.name}`;
}

function discussionHref({ repository, target }: Notification) {
  switch (target.kind) {
    case "issue":
      return repoHref(repository, {
        view: "issues",
        issue: String(target.number),
      });
    case "issue_comment":
      return `${repoHref(repository, { view: "issues", issue: String(target.issue) })}#issuecomment-${target.number}`;
    case "pull":
      return repoHref(repository, {
        view: "pulls",
        pull: String(target.number),
      });
    case "pull_comment":
    case "pull_review": {
      const hash =
        target.kind === "pull_comment" ? "issuecomment" : "pullrequestreview";
      return `${repoHref(repository, { view: "pulls", pull: String(target.pull) })}#${hash}-${target.number}`;
    }
    case "pull_thread":
    case "pull_reply":
      return repoHref(repository, {
        view: "pulls",
        pull: String(target.pull),
        pull_tab: "files",
        thread: String(
          target.kind === "pull_reply" ? target.thread : target.number,
        ),
        reply: target.kind === "pull_reply" ? String(target.number) : undefined,
      });
  }
}

function inboxHref(unread: boolean, repository: string) {
  const query = new URLSearchParams();
  if (!unread) query.set("tab", "all");
  if (repository) query.set("repository", repository);
  return `/-/inbox${query.size ? `?${query}` : ""}`;
}

export function Inbox({ csrf, url }: { csrf: string; url: URL }) {
  const unread = url.searchParams.get("tab") !== "all";
  const repository = url.searchParams.get("repository") ?? "";
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
                href={inboxHref(isUnread, repository)}
                aria-current={unread === isUnread ? "page" : undefined}
              >
                {isUnread ? "Unread" : "All"}
              </Link>
            </Button>
          ))}
        </nav>
      </div>
      {repository && (
        <Link className="w-fit underline" href={inboxHref(unread, "")}>
          Clear repository filter
        </Link>
      )}
      <Notifications
        key={inboxHref(unread, repository)}
        csrf={csrf}
        unread={unread}
        repository={repository}
      />
    </section>
  );
}

function Notifications({
  csrf,
  unread,
  repository,
}: {
  csrf: string;
  unread: boolean;
  repository: string;
}) {
  const [cursor, setCursor] = useState<string>();
  const [older, setOlder] = useState<Notification[]>([]);
  const [marked, setMarked] = useState<Record<string, boolean>>({});
  const [announcement, setAnnouncement] = useState("");
  const filters = new URLSearchParams({ unread: String(unread) });
  if (repository) filters.set("repository", repository);
  const initial = useRequest<InboxPage>(`/api/inbox?${filters}`);
  if (cursor) filters.set("cursor", cursor);
  const page = useRequest<InboxPage>(cursor ? `/api/inbox?${filters}` : null);
  const repositoryFilter = useRef<HTMLSelectElement>(null);
  return (
    <Result state={initial} timingLabel="Notification inbox">
      {(first) => {
        const items = [...first.items, ...older, ...(page.data?.items ?? [])];
        const next = (cursor ? page.data : first)?.next;
        const visible = items.filter(
          (item) => !unread || !(marked[itemKey(item)] ?? item.read),
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
                  navigate(inboxHref(unread, event.target.value))
                }
              >
                <NativeSelectOption value="">
                  All repositories
                </NativeSelectOption>
                {first.repositories.map((repo) => (
                  <NativeSelectOption key={repo.id} value={repo.id}>
                    {titleOf(repo)}
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
                    key={itemKey(item)}
                    item={item}
                    read={marked[itemKey(item)] ?? item.read}
                    csrf={csrf}
                    onMarked={(read) => {
                      if (unread && read) repositoryFilter.current?.focus();
                      setAnnouncement(
                        read ? "Marked as read" : "Marked as unread",
                      );
                      setMarked((current) => ({
                        ...current,
                        [itemKey(item)]: read,
                      }));
                    }}
                  />
                ))}
              </ul>
            ) : (
              <p className="rounded-lg border px-4 py-8 text-center text-muted-foreground">
                {first.scope_required
                  ? "Choose a repository to view its notifications."
                  : unread
                    ? "No unread notifications."
                    : "No notifications yet."}
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

function itemKey(item: Notification) {
  return `${item.repository.id}:${item.event}`;
}

function NotificationRow({
  item,
  read,
  csrf,
  onMarked,
}: {
  item: Notification;
  read: boolean;
  csrf: string;
  onMarked: (read: boolean) => void;
}) {
  const mutation = useMutation(csrf, null);
  const review = item.reason === "review_requested";
  const Icon = review ? GitPullRequestIcon : AtSignIcon;
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
          href={discussionHref(item)}
          className="break-words font-semibold hover:underline"
        >
          {item.title}
        </Link>
        <p className="text-muted-foreground">
          <Author author={item.actor} />{" "}
          {review ? "requested your review" : "mentioned you"}{" "}
          <span aria-hidden="true">·</span> <RelativeTime value={item.at} />
        </p>
        {mutation.error && <p role="alert">{mutation.error}</p>}
      </div>
      <Button
        className="col-start-2 w-fit sm:col-start-3"
        variant="outline"
        size="sm"
        disabled={mutation.pending}
        onClick={async () => {
          const result = await mutation.run<{ read: boolean }>(
            `/api/inbox/${item.repository.id}/${item.event}`,
            "PATCH",
            { at: item.at, read: !read },
          );
          if (result) onMarked(result.read);
        }}
      >
        {read ? "Mark as unread" : "Mark as read"}
      </Button>
    </li>
  );
}
