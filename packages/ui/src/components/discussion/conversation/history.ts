import { useCallback, useEffect, useRef, useState } from "react";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import {
  entryKey,
  entryOrder,
  entryVersion,
  type Entry,
  type Page,
  type Comment,
} from "./model";

type Request = {
  scope: string;
  kind: "initial" | "older" | "newer";
  cursor?: string;
};
export function useConversation(
  repo: Repository,
  target: string,
  focus: { id: string; query: string } | undefined,
  active: boolean,
) {
  const scope = focus?.id ?? "";
  const [position, setPosition] = useState<Request>({ scope, kind: "initial" });
  const kind = position.scope === scope ? position.kind : "initial";
  const cursor = position.scope === scope ? position.cursor : undefined;
  const resource = useRequest<Page>(
    active
      ? endpoint(repo, `${target}/conversation`, {
          focus: kind === "initial" ? focus?.query : undefined,
          before: kind === "older" ? cursor : undefined,
          after: kind === "newer" ? cursor : undefined,
        })
      : null,
  );
  const [cache, setCache] = useState<Record<string, Entry>>({});
  const [window, setWindow] = useState<Set<string>>(new Set());
  const [settled, setSettled] = useState<Page>();
  const [bounds, setBounds] = useState<
    Pick<Page, "start" | "end" | "has_earlier" | "has_later">
  >({ start: null, end: null, has_earlier: false, has_later: false });
  function upsert(entry: Entry) {
    setCache((current) => {
      const key = entryKey(entry),
        previous = current[key];
      return previous && entryVersion(previous) > entryVersion(entry)
        ? current
        : { ...current, [key]: entry };
    });
  }
  useEffect(() => {
    const page = resource.data;
    if (!page) return;
    setCache((current) => {
      const next = { ...current };
      for (const entry of page.items) {
        const key = entryKey(entry),
          previous = next[key];
        if (!previous || entryVersion(previous) <= entryVersion(entry))
          next[key] = entry;
      }
      return next;
    });
    setWindow(
      (current) =>
        new Set([
          ...(kind === "initial" ? [] : current),
          ...page.items.map(entryKey),
        ]),
    );
    setBounds((current) => ({
      start:
        kind === "newer"
          ? current.start
          : kind === "initial"
            ? page.start
            : (page.start ?? current.start),
      end:
        kind === "older"
          ? current.end
          : kind === "initial"
            ? page.end
            : (page.end ?? current.end),
      has_earlier: kind === "newer" ? current.has_earlier : page.has_earlier,
      has_later: kind === "older" ? current.has_later : page.has_later,
    }));
    setSettled(page);
  }, [resource.data, kind, scope]);
  // Applying the response and enabling controls share a commit. A pointer action
  // cannot begin in the frame before newly loaded history moves the footer.
  const loading =
    resource.loading || Boolean(resource.data && resource.data !== settled);
  const refresh = useCallback(() => {
    setPosition({ scope, kind: "initial" });
    resource.retry();
  }, [scope, resource.retry]);
  const shown = useRef(active);
  useEffect(() => {
    if (active && !shown.current) refresh();
    shown.current = active;
  }, [active, refresh]);
  return {
    ...resource,
    loading,
    entries: Object.values(cache).sort(entryOrder),
    window,
    head: settled?.head_oid ?? null,
    older: bounds.has_earlier,
    newer: bounds.has_later,
    initialized: Boolean(settled || resource.error),
    upsert,
    posted(value: Comment) {
      if (bounds.has_later) return;
      const entry: Entry = { kind: "comment", value };
      upsert(entry);
      setWindow((current) => new Set([...current, entryKey(entry)]));
      if (bounds.end) {
        setPosition({ scope, kind: "newer", cursor: bounds.end });
        resource.retry();
      } else refresh();
    },
    refresh,
    earlier() {
      if (!loading && bounds.has_earlier && bounds.start)
        setPosition({ scope, kind: "older", cursor: bounds.start });
    },
    later() {
      if (!loading && bounds.has_later && bounds.end)
        setPosition({ scope, kind: "newer", cursor: bounds.end });
    },
  };
}
