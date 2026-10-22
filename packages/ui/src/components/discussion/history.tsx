import { useCallback, useEffect, useState } from "react";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";

interface Entry {
  number: number;
  version: number;
}
interface Page<T> {
  items: T[];
  next: number | null;
}

export function useDiscussionHistory<
  T extends Entry,
  P extends Page<T> = Page<T>,
>(repo: Repository, target: string, focus?: number) {
  const [before, setBefore] = useState<number>();
  const page = useRequest<P>(
    endpoint(repo, target, { before: before?.toString() }),
  );
  // A permalink is a point read, independent of the history cursor. Opening an
  // old contribution must not download every intervening page or skip a range.
  const linked = useRequest<T>(
    focus ? endpoint(repo, `${target}/${focus}`) : null,
  );
  const [items, setItems] = useState<Record<number, T>>({});
  const [settled, setSettled] = useState<P>();
  const upsert = useCallback((incoming: T[]) => {
    setItems((old) => {
      const merged = { ...old };
      for (const item of incoming) {
        // In-flight pages must not replace a newer successful edit/point read.
        if (!merged[item.number] || merged[item.number].version <= item.version)
          merged[item.number] = item;
      }
      return merged;
    });
  }, []);
  useEffect(() => {
    if (page.data) {
      setSettled(page.data);
      upsert(page.data.items);
    }
  }, [page.data, upsert]);
  useEffect(() => {
    if (linked.data) upsert([linked.data]);
  }, [linked.data, upsert]);
  return {
    ...page,
    data: page.data ?? settled,
    items: Object.values(items),
    next: page.data?.next ?? null,
    linked,
    upsert,
    refresh() {
      setBefore(undefined);
      page.retry();
      if (focus) linked.retry();
    },
    loadEarlier() {
      if (!page.loading && page.data?.next) setBefore(page.data.next);
    },
  };
}

export function HistoryControls({
  label,
  history,
}: {
  label: "comments" | "reviews";
  history: Pick<
    ReturnType<typeof useDiscussionHistory>,
    "loading" | "error" | "retry" | "next" | "loadEarlier" | "linked"
  >;
}) {
  if (
    !history.next &&
    !history.loading &&
    !history.error &&
    !history.linked.loading &&
    !history.linked.error
  )
    return null;
  return (
    <div
      role="group"
      className="flex flex-col gap-2"
      aria-label={`${label} history`}
    >
      {history.next !== null && (
        <Button
          variant="outline"
          className="self-start"
          disabled={history.loading}
          onClick={history.loadEarlier}
        >
          Load earlier {label}
        </Button>
      )}
      {history.loading && (
        <p
          role="status"
          className="flex items-center gap-2 text-muted-foreground"
        >
          <Spinner aria-hidden="true" /> Loading {label}…
        </p>
      )}
      {history.error && (
        <HistoryFailure
          title={`Unable to load ${label}`}
          message={history.error}
          retry={history.retry}
        />
      )}
      {history.linked.loading && (
        <p
          role="status"
          className="flex items-center gap-2 text-muted-foreground"
        >
          <Spinner aria-hidden="true" /> Loading linked contribution…
        </p>
      )}
      {history.linked.error && (
        <HistoryFailure
          title="Unable to open this contribution"
          message={history.linked.error}
          retry={history.linked.retry}
        />
      )}
    </div>
  );
}

function HistoryFailure({
  title,
  message,
  retry,
}: {
  title: string;
  message: string;
  retry: () => void;
}) {
  return (
    <Alert variant="destructive">
      <AlertTitle>{title}</AlertTitle>
      <AlertDescription>
        <p>{message}</p>
        <Button variant="outline" size="sm" onClick={retry}>
          Try again
        </Button>
      </AlertDescription>
    </Alert>
  );
}
