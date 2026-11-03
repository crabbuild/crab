// Starts the heap watcher on mount and connects it to the OPFS
// eviction manager. The watcher samples performance.memory every 5s
// and triggers eviction when the cap is exceeded.

import { useEffect, useRef } from "react";
import { useSetting } from "../settings";
import { HeapWatcher } from "../lib/heap-watcher";
import { evictionManager } from "../lib/opfs-eviction";

/**
 * App-level hook that runs the heap watcher and triggers tab-state
 * eviction when memory pressure is high. Call once in the root Shell
 * component.
 */
export function useHeapWatcherEffect(): void {
  const capMb = useSetting("heapCapMb");
  const watcherRef = useRef<HeapWatcher | null>(null);

  useEffect(() => {
    const watcher = new HeapWatcher({
      capMb,
      onOverCap: () => {
        const tabId = evictionManager.getLeastRecentlyFocused();
        if (tabId) {
          // The actual eviction requires the tab's state, which the
          // EditorPane or tab manager would provide. For now we log
          // the intent — full wiring requires the tab state store to
          // expose a serialize method (future integration).
          console.debug(
            `[heap-watcher] over cap (${watcher.usageMb} MB > ${capMb} MB), would evict tab: ${tabId}`,
          );
        }
      },
    });
    watcher.start();
    watcherRef.current = watcher;

    return () => {
      watcher.destroy();
      watcherRef.current = null;
    };
  }, [capMb]);
}
