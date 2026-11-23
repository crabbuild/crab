import { useRef, useState } from "react";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { useRequest } from "@/lib/api";
import type { Address } from "./actions";
import { BulkUnwatch } from "./bulk-unwatch";
import { refreshNotificationBadge } from "./index";
import {
  RepositoryWatch,
  watchTopics,
  type RepositoryPreference,
} from "./preferences";

type Watch = {
  repository: Address;
  preference: RepositoryPreference;
};

export type WatchPage = { items: Watch[]; next: string | null };

export function WatchedRepositories({ csrf }: { csrf: string }) {
  const heading = useRef<HTMLHeadingElement>(null);
  const [after, setAfter] = useState<string>();
  const [previous, setPrevious] = useState<Watch[]>([]);
  const [saved, setSaved] = useState<Record<string, RepositoryPreference>>({});
  const [announcement, setAnnouncement] = useState("");
  const path = "/api/notifications/watched";
  const page = useRequest<WatchPage>(
    after ? `${path}?${new URLSearchParams({ after })}` : path,
  );
  const rows = [...previous, ...(page.data?.items ?? [])].flatMap(
    ({ repository, preference }) => {
      const topics = watchTopics(saved[repository.id] ?? preference);
      return topics.length ? [{ repository, topics }] : [];
    },
  );

  const owners = [
    ...new Set(rows.map(({ repository }) => repository.owner)),
  ].sort();

  function preferenceSaved(
    repository: Address,
    preference: RepositoryPreference,
  ) {
    setSaved((current) => ({ ...current, [repository.id]: preference }));
    const watching = watchTopics(preference).length > 0;
    const label = `${repository.owner}/${repository.name}`;
    setAnnouncement(
      watching ? `Watch updated for ${label}.` : `Stopped watching ${label}.`,
    );
    if (!watching) heading.current?.focus();
  }

  function startOver() {
    setPrevious([]);
    setSaved({});
    setAfter(undefined);
    page.retry();
    heading.current?.focus();
  }

  return (
    <section
      className="mx-auto grid max-w-4xl gap-5 px-4 py-6 sm:px-8"
      aria-labelledby="watched-title"
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h1
          ref={heading}
          id="watched-title"
          tabIndex={-1}
          className="text-2xl font-semibold tracking-tight"
        >
          Watched repositories
        </h1>
        <div className="flex flex-wrap gap-2">
          <BulkUnwatch
            csrf={csrf}
            owners={owners}
            onFinished={() => {
              refreshNotificationBadge();
              startOver();
            }}
          />
          <Button asChild variant="outline">
            <Link href="/-/notifications">Back to notifications</Link>
          </Button>
        </div>
      </div>
      <p role="status" className="sr-only">
        {announcement}
      </p>
      {rows.length > 0 && (
        <ul className="divide-y overflow-hidden rounded-lg border">
          {rows.map(({ repository, topics }) => {
            const { id, name, owner } = repository;
            return (
              <li
                key={id}
                className="flex items-center justify-between gap-3 px-4 py-3"
              >
                <div className="min-w-0">
                  <Link
                    className="break-words font-medium hover:underline"
                    href={`/${encodeURIComponent(owner)}/${encodeURIComponent(name)}?${new URLSearchParams({ repository_id: id })}`}
                  >
                    {owner}/{name}
                  </Link>
                  <div className="text-sm text-muted-foreground">
                    {topics.join(" · ")}
                  </div>
                </div>
                <RepositoryWatch
                  repo={repository}
                  csrf={csrf}
                  onSaved={(preference) =>
                    preferenceSaved(repository, preference)
                  }
                />
              </li>
            );
          })}
        </ul>
      )}
      {page.loading && (
        <p className="notice" role="status">
          <Spinner /> Loading watched repositories…
        </p>
      )}
      {page.error && (
        <div className="notice error" role="alert">
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
            ? "No watched repositories found so far."
            : "You aren’t watching any repositories."}
        </p>
      )}
      {page.data?.next && (
        <Button
          variant="outline"
          className="justify-self-start"
          onClick={() => {
            const batch = page.data;
            if (!batch?.next) return;
            setPrevious((current) => [...current, ...batch.items]);
            setAfter(batch.next);
          }}
        >
          Load more repositories
        </Button>
      )}
    </section>
  );
}
