import { useEffect, useRef, useState } from "react";
import { onAgentEvent, ProgressPartial } from "../ipc";

/**
 * Subscribe to `progress/partial` notifications matching a given
 * `(opId, kind)` pair and accumulate chunks into an array.
 *
 * Returns the partial results collected so far. The array resets
 * whenever `opId` changes (including from a non-null value to a
 * different non-null value). When `opId` is null, no subscription
 * is active and the hook returns an empty array.
 *
 * Usage:
 * ```ts
 * const partials = usePartialResults<DirEntry>(opId, "list_entry");
 * ```
 */
export function usePartialResults<T = unknown>(
  opId: string | null,
  kind: string,
): T[] {
  const [chunks, setChunks] = useState<T[]>([]);
  const opIdRef = useRef(opId);

  // Reset accumulated chunks when opId changes.
  useEffect(() => {
    if (opIdRef.current !== opId) {
      opIdRef.current = opId;
      setChunks([]);
    }
  }, [opId]);

  useEffect(() => {
    if (!opId) {
      setChunks([]);
      return;
    }

    const off = onAgentEvent((evt) => {
      if (evt.method !== "progress/partial") return;
      const params = evt.params as ProgressPartial;
      if (params.op_id !== opId || params.kind !== kind) return;
      setChunks((prev) => [...prev, params.chunk as T]);
    });

    return off;
  }, [opId, kind]);

  return chunks;
}
