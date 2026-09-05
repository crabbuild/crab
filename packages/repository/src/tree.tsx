import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { FileTree, useFileTree } from "@pierre/trees/react";
import { Button } from "@primer/react";
import { FileIcon, SearchIcon } from "@primer/octicons-react";
import {
  displayHex,
  endpoint,
  parentHex,
  request,
  type Entry,
  type Page,
  type Repository,
  type SearchResults,
} from "./api";
import { compareFileItems } from "./entry-sort";

type SearchState = {
  query: string;
  loading: boolean;
  items: Entry[];
  truncated: boolean;
  error?: string;
};

export function RepositoryTree({
  repo,
  rev,
  activePath,
  activePathHex,
  focusRequest,
  onSelect,
}: {
  repo: Repository;
  rev: string;
  activePath?: string;
  activePathHex?: string;
  focusRequest: number;
  onSelect: (entry: Entry) => void;
}) {
  const entries = useRef(new Map<string, Entry>());
  const select = useRef(onSelect);
  const syncingSelection = useRef(false);
  const loadDirectory = useRef(
    (_path: string, _pathHex: string): Promise<void> => Promise.resolve(),
  );
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
    sort: (left, right) =>
      compareFileItems(
        left.basename,
        left.isDirectory,
        right.basename,
        right.isDirectory,
      ),
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
  const [query, setQuery] = useState("");
  const searchInput = useRef<HTMLInputElement>(null);
  const [activeResult, setActiveResult] = useState(0);
  const [searchState, setSearchState] = useState<SearchState>({
    query: "",
    loading: false,
    items: [],
    truncated: false,
  });
  const normalizedQuery = query.trim();
  const searchResults =
    searchState.query === normalizedQuery ? searchState.items : [];
  useLayoutEffect(() => {
    if (focusRequest > 0) searchInput.current?.focus();
  }, [focusRequest]);
  useEffect(() => {
    if (!normalizedQuery || !searchResults[activeResult]) return;
    document
      .getElementById(`repository-search-result-${activeResult}`)
      ?.scrollIntoView({ block: "nearest" });
  }, [activeResult, normalizedQuery, searchResults]);
  useEffect(() => {
    setActiveResult(0);
    if (!normalizedQuery) {
      setSearchState({
        query: "",
        loading: false,
        items: [],
        truncated: false,
      });
      return;
    }
    const controller = new AbortController();
    setSearchState({
      query: normalizedQuery,
      loading: true,
      items: [],
      truncated: false,
    });
    const timer = window.setTimeout(() => {
      void request<SearchResults>(
        endpoint(repo, "search", {
          rev,
          q: normalizedQuery,
          limit: "50",
        }),
        controller.signal,
      )
        .then(({ data }) => {
          if (!controller.signal.aborted)
            setSearchState({
              query: normalizedQuery,
              loading: false,
              items: data.items,
              truncated: data.truncated,
            });
        })
        .catch((failure: unknown) => {
          if (!controller.signal.aborted)
            setSearchState({
              query: normalizedQuery,
              loading: false,
              items: [],
              truncated: false,
              error:
                failure instanceof Error
                  ? failure.message
                  : "Could not search repository files",
            });
        });
    }, 200);
    return () => {
      window.clearTimeout(timer);
      controller.abort();
    };
  }, [repo.owner, repo.name, rev, normalizedQuery]);
  function openSearchResult(entry: Entry) {
    setQuery("");
    select.current(entry);
  }
  useEffect(() => {
    const controller = new AbortController();
    const loads = new Map<string, Promise<void>>();
    entries.current.clear();
    model.resetPaths([]);
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
    loadDirectory.current = load;
    void load("", "");
    return () => {
      controller.abort();
      unsubscribe();
      if (loadDirectory.current === load)
        loadDirectory.current = () => Promise.resolve();
    };
  }, [repo.owner, repo.name, rev, model, attempt]);
  useEffect(() => {
    let frame: number | undefined;
    let cancelled = false;
    void (async () => {
      await loadDirectory.current("", "");
      for (const ancestor of ancestors) {
        if (cancelled) return;
        const item = model.getItem(`${ancestor.path}/`);
        if (item && "expand" in item) item.expand();
        await loadDirectory.current(ancestor.path, ancestor.pathHex);
      }
      if (cancelled || !activePath) return;
      const item = model.getItem(activePath);
      if (!item) return;
      frame = requestAnimationFrame(() => {
        if (cancelled) return;
        syncingSelection.current = true;
        item.select();
        syncingSelection.current = false;
        model.scrollToPath(activePath, { offset: "center" });
      });
    })();
    return () => {
      cancelled = true;
      if (frame !== undefined) cancelAnimationFrame(frame);
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
          role="combobox"
          aria-autocomplete="list"
          aria-expanded={searchResults.length > 0}
          aria-controls={
            searchResults.length ? "repository-search-results" : undefined
          }
          aria-activedescendant={
            searchResults[activeResult]
              ? `repository-search-result-${activeResult}`
              : undefined
          }
          placeholder="Go to file"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Escape") {
              setQuery("");
              event.currentTarget.blur();
              return;
            }
            if (event.key === "ArrowDown" && searchResults.length) {
              event.preventDefault();
              setActiveResult((value) =>
                Math.min(value + 1, searchResults.length - 1),
              );
            } else if (event.key === "ArrowUp" && searchResults.length) {
              event.preventDefault();
              setActiveResult((value) => Math.max(value - 1, 0));
            } else if (event.key === "Enter" && searchResults[activeResult]) {
              event.preventDefault();
              openSearchResult(searchResults[activeResult]);
            }
          }}
        />
        <kbd aria-hidden="true">T</kbd>
      </label>
      {normalizedQuery ? (
        <div
          className="tree-search-results"
          aria-label="Repository file search results"
        >
          {searchState.loading ? (
            <p role="status">Searching repository…</p>
          ) : searchState.error ? (
            <p className="error" role="alert">
              {searchState.error}
            </p>
          ) : searchResults.length ? (
            <>
              <ul id="repository-search-results" role="listbox">
                {searchResults.map((entry, index) => (
                  <li
                    id={`repository-search-result-${index}`}
                    key={entry.path_hex}
                    role="option"
                    className={index === activeResult ? "active" : undefined}
                    aria-selected={index === activeResult}
                    onMouseDown={(event) => event.preventDefault()}
                    onMouseEnter={() => setActiveResult(index)}
                    onClick={() => openSearchResult(entry)}
                  >
                    <FileIcon aria-hidden="true" />
                    <span>{entry.path}</span>
                  </li>
                ))}
              </ul>
              {searchState.truncated && (
                <p role="status">
                  Showing the first 50 matches. Refine your search.
                </p>
              )}
            </>
          ) : (
            <p>No files match “{normalizedQuery}”.</p>
          )}
        </div>
      ) : (
        <>
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
      )}
    </>
  );
}
