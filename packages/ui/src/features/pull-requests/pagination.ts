import { useRequest } from "@/lib/api";
import { useEffect, useState } from "react";

import type { Page } from "./model";

export function usePagedRequest<T>(url: (before?: number) => string) {
  const [before, setBefore] = useState<number>();
  const [items, setItems] = useState<T[]>([]);
  const state = useRequest<Page<T>>(url(before));
  useEffect(() => {
    if (!state.data) return;
    setItems((current) =>
      before === undefined
        ? state.data!.items
        : [...current, ...state.data!.items],
    );
  }, [before, state.data]);
  function retry() {
    setBefore(undefined);
    setItems([]);
    state.retry();
  }
  function loadMore() {
    if (state.data?.next !== null && state.data?.next !== undefined)
      setBefore(state.data.next);
  }
  return {
    ...state,
    items,
    next: state.data?.next ?? null,
    retry,
    loadMore,
  };
}
