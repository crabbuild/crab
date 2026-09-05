import { useEffect, useMemo, useRef, useState } from "react";
import { FileTree, useFileTree, useFileTreeSearch } from "@pierre/trees/react";
import { Button } from "@primer/react";
import { SearchIcon } from "@primer/octicons-react";
import {
  displayHex,
  endpoint,
  parentHex,
  request,
  type Entry,
  type Page,
  type Repository,
} from "./api";

export function RepositoryTree({
  repo,
  rev,
  activePath,
  activePathHex,
  onSelect,
}: {
  repo: Repository;
  rev: string;
  activePath?: string;
  activePathHex?: string;
  onSelect: (entry: Entry) => void;
}) {
  const entries = useRef(new Map<string, Entry>());
  const select = useRef(onSelect);
  const syncingSelection = useRef(false);
  select.current = onSelect;
  const [error, setError] = useState("");
  const [attempt, setAttempt] = useState(0);
  const [pending, setPending] = useState(0);
  const ancestors = useMemo(() => {
    const paths: { path: string; pathHex: string }[] = [];
    let pathHex = parentHex(activePathHex ?? "");
    while (pathHex) {
      paths.unshift({ path: displayHex(pathHex), pathHex });
      pathHex = parentHex(pathHex);
    }
    return paths;
  }, [activePathHex]);
  const { model } = useFileTree({
    paths: [],
    initialExpansion: "closed",
    initialExpandedPaths: ancestors.map(({ path }) => `${path}/`),
    initialSelectedPaths: activePath ? [activePath] : [],
    flattenEmptyDirectories: false,
    fileTreeSearchMode: "hide-non-matches",
    search: true,
    density: "default",
    itemHeight: 32,
    icons: { set: "minimal", colored: false },
    renaming: false,
    dragAndDrop: false,
    // Pierre exposes one leading icon slot for directories. GitHub uses both a
    // disclosure chevron and a folder, so its supported CSS escape hatch adds it.
    unsafeCSS: `
      [data-file-tree-search-container] {
        display: none;
      }
      [data-type="item"][data-item-selected="true"]::after {
        background: var(--fgColor-accent);
        border-radius: 0 6px 6px 0;
        content: "";
        inset-block: 0;
        inset-inline-start: calc(-1 * var(--trees-padding-inline));
        position: absolute;
        width: 3px;
      }
      [data-type="item"][data-item-selected="true"] > [data-item-section="icon"] {
        color: var(--trees-fg-muted);
      }
      [data-type="item"][data-item-type="folder"] > [data-item-section="content"] {
        align-items: center;
        display: flex;
        gap: 8px;
      }
      [data-type="item"][data-item-type="folder"] > [data-item-section="content"]::before {
        background: var(--trees-icon-blue);
        content: "";
        flex: 0 0 16px;
        height: 16px;
        -webkit-mask: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'%3E%3Cpath d='M1.75 1A1.75 1.75 0 0 0 0 2.75v10.5C0 14.216.784 15 1.75 15h12.5A1.75 1.75 0 0 0 16 13.25v-8.5A1.75 1.75 0 0 0 14.25 3H7.5a.25.25 0 0 1-.2-.1l-.9-1.2C6.07 1.26 5.55 1 5 1H1.75Z'/%3E%3C/svg%3E") center / 16px 16px no-repeat;
        mask: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'%3E%3Cpath d='M1.75 1A1.75 1.75 0 0 0 0 2.75v10.5C0 14.216.784 15 1.75 15h12.5A1.75 1.75 0 0 0 16 13.25v-8.5A1.75 1.75 0 0 0 14.25 3H7.5a.25.25 0 0 1-.2-.1l-.9-1.2C6.07 1.26 5.55 1 5 1H1.75Z'/%3E%3C/svg%3E") center / 16px 16px no-repeat;
        width: 16px;
      }
    `,
    onSelectionChange(paths) {
      if (syncingSelection.current) return;
      const entry = entries.current.get((paths[0] ?? "").replace(/\/$/, ""));
      if (entry) select.current(entry);
    },
  });
  const search = useFileTreeSearch(model);
  const searchInput = useRef<HTMLInputElement>(null);
  useEffect(() => {
    const controller = new AbortController();
    const loads = new Map<string, Promise<void>>();
    entries.current.clear();
    model.resetPaths([], {
      initialExpandedPaths: ancestors.map(({ path }) => `${path}/`),
    });
    setError("");
    setPending(0);
    async function load(path: string, pathHex: string) {
      if (controller.signal.aborted) return;
      const existing = loads.get(path);
      if (existing) return existing;
      const pendingLoad = (async () => {
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
            for (const entry of additions)
              entries.current.set(entry.path, entry);
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
              error instanceof Error
                ? error.message
                : "Could not load directory",
            );
        } finally {
          if (!controller.signal.aborted) setPending((value) => value - 1);
        }
      })();
      loads.set(path, pendingLoad);
      return pendingLoad;
    }
    const unsubscribe = model.subscribe(() => {
      for (const row of model.getVisibleRows(0, model.getVisibleCount())) {
        if (row.kind !== "directory" || !row.isExpanded) continue;
        const entry = entries.current.get(row.path.replace(/\/$/, ""));
        if (entry) void load(entry.path, entry.path_hex);
      }
    });
    void (async () => {
      await load("", "");
      for (const ancestor of ancestors) {
        const item = model.getItem(`${ancestor.path}/`);
        if (item && "expand" in item) item.expand();
        await load(ancestor.path, ancestor.pathHex);
      }
      if (controller.signal.aborted || !activePath) return;
      requestAnimationFrame(() => {
        if (controller.signal.aborted) return;
        const item = model.getItem(activePath);
        if (!item) return;
        syncingSelection.current = true;
        item.select();
        syncingSelection.current = false;
        model.scrollToPath(activePath, { offset: "center" });
      });
    })();
    return () => {
      controller.abort();
      unsubscribe();
    };
  }, [repo.owner, repo.name, rev, model, attempt, activePath, ancestors]);
  return (
    <>
      <label className="tree-search" htmlFor="repository-tree-search">
        <SearchIcon aria-hidden="true" />
        <input
          ref={searchInput}
          id="repository-tree-search"
          type="search"
          placeholder="Go to file"
          value={search.value}
          onFocus={() => search.open(search.value)}
          onChange={(event) =>
            search.isOpen
              ? search.setValue(event.target.value)
              : search.open(event.target.value)
          }
          onKeyDown={(event) => {
            if (event.key === "Escape") {
              search.setValue("");
              search.close();
              event.currentTarget.blur();
            }
            if (event.key !== "Enter" || search.matchingPaths.length !== 1)
              return;
            const entry = entries.current.get(
              search.matchingPaths[0].replace(/\/$/, ""),
            );
            if (entry) select.current(entry);
          }}
        />
        <kbd aria-hidden="true">T</kbd>
      </label>
      <FileTree
        model={model}
        className="repository-tree"
        aria-label="Repository files"
      />
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
