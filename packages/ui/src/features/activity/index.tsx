import { Spinner } from "@/components/ui/spinner";
import { Button } from "@/components/ui/button";
import { useEffect, useRef, useState } from "react";

import type { ActivityEvent } from "@/components/discussion/conversation/model";
import { ActivityItem } from "@/components/discussion/conversation/activity";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";
import { RequestWidget } from "@/components/request/result";

interface Page {
  items: ActivityEvent[];
  next: number | null;
}

export default function RepositoryActivity({ repo }: { repo: Repository }) {
  return (
    <section className="activity-page">
      <div className="section-heading">
        <h2>Activity</h2>
        <Link href={repoHref(repo)}>Back to code</Link>
      </div>
      <ActivityTimeline repo={repo} />
    </section>
  );
}

export function ActivityTimeline({
  repo,
  target,
}: {
  repo: Repository;
  target?: string;
}) {
  const [before, setBefore] = useState<number>();
  const [items, setItems] = useState<ActivityEvent[]>([]);
  const [next, setNext] = useState<number | null>(null);
  const heading = useRef<HTMLHeadingElement>(null);
  const resource = useRequest<Page>(
    endpoint(repo, target ? `${target}/activity` : "activity", {
      before: before?.toString(),
      limit: "30",
    }),
  );
  useEffect(() => {
    if (!resource.data) return;
    const page = resource.data;
    setItems((previous) => {
      const events = new Map(
        (before === undefined ? [] : previous).map((event) => [
          event.number,
          event,
        ]),
      );
      for (const event of page.items) events.set(event.number, event);
      return [...events.values()].sort((a, b) => b.number - a.number);
    });
    setNext(page.next);
    if (before !== undefined && page.next === null) heading.current?.focus();
  }, [before, resource.data]);
  function refresh() {
    setBefore(undefined);
    resource.retry();
  }
  return (
    <RequestWidget timing={resource.timing} label="Activity">
      <section className="panel activity-panel" aria-label="Activity">
        <div className="section-heading">
          <h3 ref={heading} tabIndex={-1}>
            Recent activity
          </h3>
          <Button
            variant="outline"
            size="sm"
            disabled={resource.loading}
            onClick={refresh}
          >
            Refresh activity
          </Button>
        </div>
        {resource.loading && (
          <p className="muted" role="status">
            <Spinner /> Loading activity…
          </p>
        )}
        {resource.error && (
          <div className="notice error" role="alert">
            <p>{resource.error}</p>
            <Button variant="outline" size="sm" onClick={resource.retry}>
              Retry activity
            </Button>
          </div>
        )}
        {!resource.loading && !resource.error && !items.length && (
          <p className="muted">
            No activity recorded yet. New changes will appear here.
          </p>
        )}
        <ol className="activity-events" aria-label="Activity, newest first">
          {items.map((event) => (
            <ActivityItem key={event.number} repo={repo} event={event} />
          ))}
        </ol>
        {next !== null && (
          <Button
            variant="outline"
            size="sm"
            disabled={resource.loading}
            onClick={() => setBefore(next)}
          >
            Load older activity
          </Button>
        )}
      </section>
    </RequestWidget>
  );
}
