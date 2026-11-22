import { useRef, useState } from "react";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { navigate, useRequest } from "@/lib/api";
import type { Address } from "./actions";
import {
  DiscussionWatchButton,
  discussionModeLabel,
  type DiscussionPreference,
} from "./preferences";

type Kind = "issue" | "pull";
type Subscription = {
  repository: Address;
  subject: { kind: Kind; number: number };
  title: string;
  state: "open" | "closed" | "merged";
  preference: DiscussionPreference;
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
  function filter(repository: string, kind: string) {
    const query = new URLSearchParams();
    if (repository) query.set("repository", repository);
    if (kind) query.set("kind", kind);
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
          <NativeSelect
            value={repository}
            onChange={(event) => filter(event.target.value, selectedKind)}
          >
            <NativeSelectOption value="">All repositories</NativeSelectOption>
            {repository &&
              !repositories.some(({ id }) => id === repository) && (
                <NativeSelectOption value={repository}>
                  Selected repository
                </NativeSelectOption>
              )}
            {repositories.map(({ id, owner, name }) => (
              <NativeSelectOption key={id} value={id}>
                {owner}/{name}
              </NativeSelectOption>
            ))}
          </NativeSelect>
        </label>
        <label className="grid gap-1 text-sm font-medium">
          Discussion type
          <NativeSelect
            value={selectedKind}
            onChange={(event) => filter(repository, event.target.value)}
          >
            <NativeSelectOption value="">
              Issues and pull requests
            </NativeSelectOption>
            <NativeSelectOption value="issue">Issues</NativeSelectOption>
            <NativeSelectOption value="pull">Pull requests</NativeSelectOption>
          </NativeSelect>
        </label>
      </div>
      <SubscriptionList
        key={`${repository}:${selectedKind}`}
        csrf={csrf}
        repository={repository}
        kind={selectedKind}
      />
    </section>
  );
}

function rowKey({ repository, subject }: Subscription) {
  return `${repository.id}:${subject.kind}:${subject.number}`;
}

function follows({ mode }: DiscussionPreference) {
  return mode === "follow" || mode === "state_changes";
}

function SubscriptionList({
  csrf,
  repository,
  kind,
}: {
  csrf: string;
  repository: string;
  kind: Kind | "";
}) {
  const [after, setAfter] = useState<string>();
  const [earlier, setEarlier] = useState<Subscription[]>([]);
  const [saved, setSaved] = useState<Record<string, DiscussionPreference>>({});
  const [announcement, setAnnouncement] = useState("");
  const list = useRef<HTMLDivElement>(null);
  const query = new URLSearchParams();
  if (after) query.set("after", after);
  if (repository) query.set("repository", repository);
  if (kind) query.set("kind", kind);
  const page = useRequest<Page>(`/api/notifications/subscriptions?${query}`);
  const rows = [...earlier, ...(page.data?.items ?? [])].filter((item) =>
    follows(saved[rowKey(item)] ?? item.preference),
  );
  function startOver() {
    setEarlier([]);
    setSaved({});
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
        <ul className="divide-y overflow-hidden rounded-lg border">
          {rows.map((item) => {
            const { repository: repo, subject, title, state, muted } = item;
            const key = rowKey(item);
            const isIssue = subject.kind === "issue";
            const preference = saved[key] ?? item.preference;
            const number = `#${subject.number}`;
            return (
              <li
                key={key}
                className="flex flex-wrap items-center justify-between gap-3 px-4 py-3"
              >
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
                </div>
                <DiscussionWatchButton
                  repo={repo}
                  csrf={csrf}
                  subject={`${isIssue ? "issues" : "pulls"}/${subject.number}`}
                  label={`Notification settings for ${repo.owner}/${repo.name} ${isIssue ? "issue" : "pull request"} ${number}`}
                  onSaved={(updated) => {
                    setSaved((current) => ({ ...current, [key]: updated }));
                    if (!follows(updated)) list.current?.focus();
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
