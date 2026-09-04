import { useEffect, useRef, useState } from "react";
import { FileTree, useFileTree } from "@pierre/trees/react";
import { Button } from "@primer/react";
import {
  endpoint,
  request,
  type Entry,
  type Page,
  type Repository,
} from "./api";

export function RepositoryTree({
  repo,
  rev,
  onSelect,
}: {
  repo: Repository;
  rev: string;
  onSelect: (entry: Entry) => void;
}) {
  const entries = useRef(new Map<string, Entry>());
  const select = useRef(onSelect);
  select.current = onSelect;
  const [error, setError] = useState("");
  const [attempt, setAttempt] = useState(0);
  const [pending, setPending] = useState(0);
  const { model } = useFileTree({
    paths: [],
    initialExpansion: "closed",
    flattenEmptyDirectories: false,
    search: true,
    renaming: false,
    dragAndDrop: false,
    onSelectionChange(paths) {
      const entry = entries.current.get((paths[0] ?? "").replace(/\/$/, ""));
      if (entry) select.current(entry);
    },
  });
  useEffect(() => {
    const controller = new AbortController();
    const loaded = new Set<string>();
    entries.current.clear();
    model.resetPaths([]);
    setError("");
    setPending(0);
    async function load(path: string, pathHex: string) {
      if (loaded.has(path) || controller.signal.aborted) return;
      loaded.add(path);
      setPending((value) => value + 1);
      try {
        let cursor: string | undefined;
        do {
          const result = await request<Page<Entry>>(
            endpoint(repo, "tree", {
              rev,
              path_hex: pathHex,
              limit: "200",
              cursor,
            }),
            controller.signal,
          );
          if (controller.signal.aborted) return;
          const additions = result.data.items.filter(
            (entry) => !entries.current.has(entry.path),
          );
          for (const entry of additions) entries.current.set(entry.path, entry);
          // Batch additions preserve focus and expansion while a directory streams in.
          model.batch(
            additions.map((entry) => ({
              type: "add",
              path: entry.path + (entry.kind === "Tree" ? "/" : ""),
            })),
          );
          cursor = result.data.next ?? undefined;
        } while (cursor);
      } catch (error: unknown) {
        if (!controller.signal.aborted)
          setError(
            error instanceof Error ? error.message : "Could not load directory",
          );
      } finally {
        if (!controller.signal.aborted) setPending((value) => value - 1);
      }
    }
    const unsubscribe = model.subscribe(() => {
      for (const row of model.getVisibleRows(0, model.getVisibleCount())) {
        if (row.kind !== "directory" || !row.isExpanded) continue;
        const entry = entries.current.get(row.path.replace(/\/$/, ""));
        if (entry) void load(entry.path, entry.path_hex);
      }
    });
    void load("", "");
    return () => {
      controller.abort();
      unsubscribe();
    };
  }, [repo.owner, repo.name, rev, model, attempt]);
  return (
    <>
      <FileTree
        model={model}
        className="repository-tree"
        aria-label="Repository files"
      />
      <p className="tree-hint">
        Expand folders to load files. Search covers loaded folders.
      </p>
      {pending > 0 && (
        <p className="tree-hint" role="status">
          Loading {pending} {pending === 1 ? "folder" : "folders"}…
        </p>
      )}
      {error && (
        <div className="error tree-hint" role="alert">
          {error}
          <Button onClick={() => setAttempt((value) => value + 1)}>
            Reload tree
          </Button>
        </div>
      )}
    </>
  );
}
