import { useEffect, useRef } from "react";
import { useRequest } from "@/lib/api";
export function useTeamRead<T>(url: string | null, generation: number) {
  const state = useRequest<T>(url);
  const previous = useRef(generation);
  useEffect(() => {
    if (previous.current === generation) return;
    previous.current = generation;
    state.retry();
  }, [generation, state.retry]);
  return state;
}
