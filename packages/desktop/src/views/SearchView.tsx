import { Input } from "@/components/ui/input";
import { useCallback,useEffect,useMemo,useRef,useState } from "react";
import { useSessionScroll } from "../hooks/useSessionScroll";
import clsx from "clsx";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
CaseSensitive,
ChevronDown,
ChevronRight,
FileText,
Filter,
FolderTree,
Layers,
List,
Loader2,
Regex,
Replace,
ReplaceAll,
Search,
WholeWord
} from "lucide-react";
import { DirEntry } from "../ipc";
import { OpenRepo,useStore,getActiveWorkspace } from "../state";
import {
gitGrepCancellable,
isCancelledError,
searchReplaceCancellable,
} from "../ipc";

interface Props {
  repo: OpenRepo;
  onOpenFile: (path: string, entry: DirEntry, line?: number) => void;
  entryByPath: Map<string, DirEntry>;
}

interface Match {
  path: string;
  line: number;
  content: string;
}

interface FileGroup {
  path: string;
  items: Match[];
  collapsed: boolean;
}

/** Height of each match row in pixels. */
const MATCH_ROW_HEIGHT = 24;

/** Results from searching across all workspace repos. */
interface RepoSearchGroup {
  repoName: string;
  relativePath: string;
  matches: Match[];
  truncated: boolean;
  error?: string;
}

interface WorkspaceSearchResults {
  groups: RepoSearchGroup[];
  totalMatches: number;
}

/**
 * Threshold for virtualising a file's match list. Files with fewer
 * matches than this render directly without virtualisation overhead.
 */
const VIRTUALISE_THRESHOLD = 50;

// ── Helper: parse comma-separated glob patterns ─────────────────────
function parsePatterns(input: string): string[] {
  return input
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
}

/**
 * VS Code-style search panel with:
 * - Case sensitivity, whole word, regex toggles
 * - Replace with preserve-case option
 * - Files to include / exclude filters
 * - Tree / list view mode
 * - Collapsible file groups
 * - Per-file and per-match replace buttons
 * - Result summary ("X results in Y files")
 */
export function SearchView({ repo, onOpenFile, entryByPath }: Props) {
  const { state } = useStore();
  const workspace = getActiveWorkspace(state);

  // ── Search state ────────────────────────────────────────────────
  const [query, setQuery] = useState("");
  const [caseSensitive, setCaseSensitive] = useState(false);
  const [wholeWord, setWholeWord] = useState(false);
  const [useRegex, setUseRegex] = useState(false);

  // ── Replace state ───────────────────────────────────────────────
  const [showReplace, setShowReplace] = useState(false);
  const [replaceValue, setReplaceValue] = useState("");
  const [preserveCase, setPreserveCase] = useState(false);

  // ── Filter state ────────────────────────────────────────────────
  const [showFilters, setShowFilters] = useState(false);
  const [includeInput, setIncludeInput] = useState("");
  const [excludeInput, setExcludeInput] = useState("");

  // ── View mode ───────────────────────────────────────────────────
  const [viewMode, setViewMode] = useState<"tree" | "list">("tree");

  // ── Workspace-scoped search ─────────────────────────────────────
  const [searchAllRepos, setSearchAllRepos] = useState(false);
  const [workspaceResults, setWorkspaceResults] = useState<WorkspaceSearchResults | null>(null);
  const [workspaceLoading, setWorkspaceLoading] = useState(false);

  // ── Results ─────────────────────────────────────────────────────
  const [matches, setMatches] = useState<Match[] | null>(null);
  const [truncated, setTruncated] = useState(false);
  const [loading, setLoading] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [collapsedFiles, setCollapsedFiles] = useState<Set<string>>(new Set());

  // ── Replace feedback ────────────────────────────────────────────
  const [replacing, setReplacing] = useState(false);
  const [replaceResult, setReplaceResult] = useState<{
    replacements: number;
    files: string[];
  } | null>(null);

  const cancelRef = useRef<(() => void) | null>(null);
  const searchInputRef = useRef<HTMLInputElement>(null);
  const sessionScrollRef = useSessionScroll("search");

  // Focus search input on mount.
  useEffect(() => {
    searchInputRef.current?.focus();
  }, []);

  const run = useCallback(() => {
    cancelRef.current?.();
    cancelRef.current = null;

    if (!query.trim()) {
      setLoading(false);
      setWorkspaceLoading(false);
      setMatches(null);
      setWorkspaceResults(null);
      setErr(null);
      return;
    }

    const includePatterns = parsePatterns(includeInput);
    const excludePatterns = parsePatterns(excludeInput);

    // Workspace-scoped search: search across all L2 repos.
    if (searchAllRepos && workspace) {
      setWorkspaceLoading(true);
      setErr(null);
      setReplaceResult(null);

      const l2Repos = workspace.repos.filter(
        (r) => r.level === 2 && r.openRepoId,
      );

      if (l2Repos.length === 0) {
        setWorkspaceResults({ groups: [], totalMatches: 0 });
        setWorkspaceLoading(false);
        return;
      }

      const searchPromises = l2Repos.map((entry) => {
        const { promise } = gitGrepCancellable(entry.openRepoId!, query, {
          ignore_case: !caseSensitive,
          regex: useRegex,
          whole_word: wholeWord,
          include_patterns: includePatterns.length > 0 ? includePatterns : undefined,
          exclude_patterns: excludePatterns.length > 0 ? excludePatterns : undefined,
        });
        return promise
          .then((r) => ({
            repoName: entry.name,
            relativePath: entry.relativePath,
            matches: r.matches,
            truncated: r.truncated,
          }))
          .catch((e: unknown) => {
            if (isCancelledError(e)) throw e;
            return {
              repoName: entry.name,
              relativePath: entry.relativePath,
              matches: [] as Match[],
              truncated: false,
              error: e instanceof Error ? e.message : String(e),
            };
          });
      });

      Promise.all(searchPromises)
        .then((groups) => {
          const totalMatches = groups.reduce((sum, g) => sum + g.matches.length, 0);
          setWorkspaceResults({ groups, totalMatches });
        })
        .catch((e: unknown) => {
          if (!isCancelledError(e)) setErr(e instanceof Error ? e.message : String(e));
        })
        .finally(() => setWorkspaceLoading(false));

      return;
    }

    // Single-repo search (default behavior).
    setLoading(true);
    setErr(null);
    setReplaceResult(null);

    const { promise, cancel } = gitGrepCancellable(repo.repo_id, query, {
      ignore_case: !caseSensitive,
      regex: useRegex,
      whole_word: wholeWord,
      include_patterns: includePatterns.length > 0 ? includePatterns : undefined,
      exclude_patterns: excludePatterns.length > 0 ? excludePatterns : undefined,
    });
    cancelRef.current = cancel;
    promise
      .then((r) => {
        if (cancelRef.current !== cancel) return;
        setMatches(r.matches);
        setTruncated(r.truncated);
      })
      .catch((e: unknown) => {
        if (cancelRef.current !== cancel) return;
        if (!isCancelledError(e)) setErr(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (cancelRef.current === cancel) {
          cancelRef.current = null;
          setLoading(false);
        }
      });
  }, [repo.repo_id, query, caseSensitive, wholeWord, useRegex, includeInput, excludeInput, searchAllRepos, workspace]);

  // Debounce the search.
  useEffect(() => {
    const t = setTimeout(run, 250);
    return () => {
      clearTimeout(t);
      cancelRef.current?.();
      cancelRef.current = null;
    };
  }, [run]);

  // ── Grouped results ─────────────────────────────────────────────
  const grouped: FileGroup[] = useMemo(() => {
    if (!matches) return [];
    const by = new Map<string, Match[]>();
    for (const m of matches) {
      const arr = by.get(m.path);
      if (arr) arr.push(m);
      else by.set(m.path, [m]);
    }
    return Array.from(by, ([path, items]) => ({
      path,
      items,
      collapsed: collapsedFiles.has(path),
    })).sort((a, b) => a.path.localeCompare(b.path));
  }, [matches, collapsedFiles]);

  const totalFiles = grouped.length;
  const totalMatches = matches?.length ?? 0;

  // ── Collapse / expand ───────────────────────────────────────────
  const toggleCollapse = useCallback((path: string) => {
    setCollapsedFiles((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }, []);

  const collapseAll = useCallback(() => {
    setCollapsedFiles(new Set(grouped.map((g) => g.path)));
  }, [grouped]);

  const expandAll = useCallback(() => {
    setCollapsedFiles(new Set());
  }, []);

  // ── Replace handlers ────────────────────────────────────────────
  const handleReplaceAll = useCallback(() => {
    if (!query.trim() || !matches || matches.length === 0) return;
    setReplacing(true);
    const filePaths = [...new Set(matches.map((m) => m.path))];
    const { promise } = searchReplaceCancellable(repo.repo_id, query, replaceValue, {
      ignore_case: !caseSensitive,
      regex: useRegex,
      whole_word: wholeWord,
      preserve_case: preserveCase,
      file_paths: filePaths,
    });
    promise
      .then((r) => {
        setReplaceResult(r);
        // Re-run search to refresh results after replacement.
        setTimeout(run, 100);
      })
      .catch((e: unknown) => {
        if (!isCancelledError(e)) setErr(e instanceof Error ? e.message : String(e));
      })
      .finally(() => setReplacing(false));
  }, [repo.repo_id, query, replaceValue, caseSensitive, useRegex, wholeWord, preserveCase, matches, run]);

  const handleReplaceInFile = useCallback(
    (filePath: string) => {
      if (!query.trim()) return;
      setReplacing(true);
      const { promise } = searchReplaceCancellable(repo.repo_id, query, replaceValue, {
        ignore_case: !caseSensitive,
        regex: useRegex,
        whole_word: wholeWord,
        preserve_case: preserveCase,
        file_paths: [filePath],
      });
      promise
        .then((r) => {
          setReplaceResult(r);
          setTimeout(run, 100);
        })
        .catch((e: unknown) => {
          if (!isCancelledError(e)) setErr(e instanceof Error ? e.message : String(e));
        })
        .finally(() => setReplacing(false));
    },
    [repo.repo_id, query, replaceValue, caseSensitive, useRegex, wholeWord, preserveCase, run],
  );

  // ── Keyboard shortcut: Enter to search ──────────────────────────
  const handleSearchKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === "Enter") {
        e.preventDefault();
        run();
      }
    },
    [run],
  );

  return (
    <>
      {/* ── Search header ──────────────────────────────────────── */}
      <div className="shrink-0 border-b border-border px-3 py-2">
        {/* Title row with view mode toggles */}
        <div className="mb-2 flex items-center justify-between">
          <div className="flex items-center gap-2 text-[10px] uppercase tracking-wider text-muted-foreground">
            <Search size={11} />
            <span>Search</span>
          </div>
          <div className="flex items-center gap-0.5">
            <ToolbarButton
              icon={<ChevronRight size={12} />}
              title="Collapse All"
              active={false}
              onClick={collapseAll}
            />
            <ToolbarButton
              icon={<ChevronDown size={12} />}
              title="Expand All"
              active={false}
              onClick={expandAll}
            />
            <ToolbarButton
              icon={<List size={12} />}
              title="List View"
              active={viewMode === "list"}
              onClick={() => setViewMode("list")}
            />
            <ToolbarButton
              icon={<FolderTree size={12} />}
              title="Tree View"
              active={viewMode === "tree"}
              onClick={() => setViewMode("tree")}
            />
            <ToolbarButton
              icon={<Filter size={12} />}
              title="Toggle File Filters"
              active={showFilters}
              onClick={() => setShowFilters(!showFilters)}
            />
            {workspace && (
              <ToolbarButton
                icon={<Layers size={12} />}
                title="Search All Repos"
                active={searchAllRepos}
                onClick={() => setSearchAllRepos(!searchAllRepos)}
              />
            )}
          </div>
        </div>

        {/* Search input row */}
        <div className="flex items-center gap-1">
          {/* Toggle replace chevron */}
          <button
            type="button"
            onClick={() => setShowReplace(!showReplace)}
            className="shrink-0 rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground"
            title={showReplace ? "Hide Replace" : "Show Replace"}
          >
            {showReplace ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
          </button>

          <div className="flex flex-1 flex-col gap-1.5">
            {/* Search field + toggles */}
            <div className="flex items-center rounded border border-border bg-background focus-within:border-primary">
              <Input
                ref={searchInputRef}
                className="min-w-0 flex-1 bg-transparent px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground focus-visible:outline-none"
                placeholder="Search"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                onKeyDown={handleSearchKeyDown}
              />
              <ToggleButton
                icon={<CaseSensitive size={13} />}
                title="Match Case"
                active={caseSensitive}
                onClick={() => setCaseSensitive(!caseSensitive)}
              />
              <ToggleButton
                icon={<WholeWord size={13} />}
                title="Match Whole Word"
                active={wholeWord}
                onClick={() => setWholeWord(!wholeWord)}
              />
              <ToggleButton
                icon={<Regex size={13} />}
                title="Use Regular Expression"
                active={useRegex}
                onClick={() => setUseRegex(!useRegex)}
              />
            </div>

            {/* Replace field + buttons */}
            {showReplace && (
              <div className="flex items-center gap-1">
                <div className="flex flex-1 items-center rounded border border-border bg-background focus-within:border-primary">
                  <Input
                    className="min-w-0 flex-1 bg-transparent px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground focus-visible:outline-none"
                    placeholder="Replace"
                    value={replaceValue}
                    onChange={(e) => setReplaceValue(e.target.value)}
                  />
                  <ToggleButton
                    icon={<CaseSensitive size={13} />}
                    title="Preserve Case"
                    active={preserveCase}
                    onClick={() => setPreserveCase(!preserveCase)}
                  />
                </div>
                <button
                  type="button"
                  onClick={handleReplaceAll}
                  disabled={replacing || !query.trim() || !matches || matches.length === 0}
                  className="shrink-0 rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground disabled:opacity-40"
                  title="Replace All"
                >
                  <ReplaceAll size={14} />
                </button>
              </div>
            )}
          </div>
        </div>

        {/* Files to include / exclude (toggled via filter icon in toolbar) */}
        {showFilters && (
          <div className="mt-2 space-y-1.5 pl-[22px]">
            <div>
              <label className="text-[10px] text-muted-foreground">files to include</label>
              <Input
                className="mt-0.5 w-full rounded border border-border bg-background px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground focus-visible:outline-none focus:border-primary"
                placeholder="e.g. *.ts, src/**"
                value={includeInput}
                onChange={(e) => setIncludeInput(e.target.value)}
              />
            </div>
            <div>
              <label className="text-[10px] text-muted-foreground">files to exclude</label>
              <Input
                className="mt-0.5 w-full rounded border border-border bg-background px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground focus-visible:outline-none focus:border-primary"
                placeholder="e.g. *.lock, node_modules/**"
                value={excludeInput}
                onChange={(e) => setExcludeInput(e.target.value)}
              />
            </div>
          </div>
        )}

        {/* Result summary */}
        {matches && !loading && !searchAllRepos && (
          <div className="mt-2 text-[11px] text-muted-foreground">
            {totalMatches} result{totalMatches !== 1 ? "s" : ""} in {totalFiles} file
            {totalFiles !== 1 ? "s" : ""}
            {truncated && " (truncated)"}
          </div>
        )}

        {/* Workspace search summary */}
        {workspaceResults && !workspaceLoading && searchAllRepos && (
          <div className="mt-2 text-[11px] text-muted-foreground">
            {workspaceResults.totalMatches} result{workspaceResults.totalMatches !== 1 ? "s" : ""} across{" "}
            {workspaceResults.groups.filter((g) => g.matches.length > 0).length} repo
            {workspaceResults.groups.filter((g) => g.matches.length > 0).length !== 1 ? "s" : ""}
          </div>
        )}

        {/* Replace feedback */}
        {replaceResult && (
          <div className="mt-1 rounded border border-success/40 bg-success/10 px-2 py-1 text-[11px] text-success">
            Replaced {replaceResult.replacements} occurrence
            {replaceResult.replacements !== 1 ? "s" : ""} in{" "}
            {replaceResult.files.length} file{replaceResult.files.length !== 1 ? "s" : ""}
          </div>
        )}
      </div>

      {/* ── Results area ───────────────────────────────────────── */}
      {/* Tree mode: single outer scroller for all content.
          List mode: the virtualizer owns scrolling, so no overflow here. */}
      <div
        ref={viewMode === "tree" ? (sessionScrollRef as React.RefObject<HTMLDivElement>) : undefined}
        className={clsx("min-h-0 flex-1", viewMode === "tree" ? "overflow-auto" : "overflow-hidden")}
      >
        {(loading || workspaceLoading) && (
          <div className="flex items-center gap-2 p-3 text-xs text-muted-foreground">
            <Loader2 size={12} className="animate-spin" /> Searching…
          </div>
        )}
        {err && (
          <div className="m-3 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
            {err}
          </div>
        )}

        {/* Workspace-scoped results */}
        {searchAllRepos && workspaceResults && !workspaceLoading && (
          <>
            {workspaceResults.totalMatches === 0 && (
              <div className="p-3 text-xs text-muted-foreground">No results found across workspace repos.</div>
            )}
            <WorkspaceSearchResultsView
              results={workspaceResults}
              query={query}
              useRegex={useRegex}
              caseSensitive={caseSensitive}
              entryByPath={entryByPath}
              onOpenFile={onOpenFile}
            />
          </>
        )}

        {/* Single-repo results */}
        {!searchAllRepos && (
          <>
            {matches && matches.length === 0 && !loading && (
              <div className="p-3 text-xs text-muted-foreground">No results found.</div>
            )}

            {viewMode === "tree" && grouped.length > 0 && (
              <TreeResults
                grouped={grouped}
                query={query}
                useRegex={useRegex}
                caseSensitive={caseSensitive}
                showReplace={showReplace}
                entryByPath={entryByPath}
                onOpenFile={onOpenFile}
                onToggleCollapse={toggleCollapse}
                onReplaceInFile={handleReplaceInFile}
              />
            )}

            {viewMode === "list" && grouped.length > 0 && (
              <ListResults
                matches={matches ?? []}
                query={query}
                useRegex={useRegex}
                caseSensitive={caseSensitive}
                entryByPath={entryByPath}
                onOpenFile={onOpenFile}
              />
            )}

            {truncated && (
              <div className="px-3 py-2 text-[11px] text-muted-foreground">
                Results truncated. Refine your query to see more.
              </div>
            )}
          </>
        )}
      </div>
    </>
  );
}

// ── Workspace Search Results (grouped by repo) ─────────────────────

function WorkspaceSearchResultsView({
  results,
  query,
  useRegex,
  caseSensitive,
  entryByPath,
  onOpenFile,
}: {
  results: WorkspaceSearchResults;
  query: string;
  useRegex: boolean;
  caseSensitive: boolean;
  entryByPath: Map<string, DirEntry>;
  onOpenFile: (path: string, entry: DirEntry, line?: number) => void;
}) {
  const [collapsedRepos, setCollapsedRepos] = useState<Set<string>>(new Set());

  const toggleRepo = useCallback((relativePath: string) => {
    setCollapsedRepos((prev) => {
      const next = new Set(prev);
      if (next.has(relativePath)) next.delete(relativePath);
      else next.add(relativePath);
      return next;
    });
  }, []);

  return (
    <div className="py-1">
      {results.groups
        .filter((g) => g.matches.length > 0 || g.error)
        .map((group) => {
          const isCollapsed = collapsedRepos.has(group.relativePath);
          return (
            <div key={group.relativePath} className="mb-1">
              {/* Repo section header */}
              <button
                type="button"
                onClick={() => toggleRepo(group.relativePath)}
                className="flex w-full items-center gap-1.5 px-2 py-1 text-left hover:bg-accent"
              >
                {isCollapsed ? (
                  <ChevronRight size={12} className="shrink-0 text-muted-foreground" />
                ) : (
                  <ChevronDown size={12} className="shrink-0 text-muted-foreground" />
                )}
                <Layers size={11} className="shrink-0 text-primary" />
                <span className="text-[11px] font-medium text-foreground">
                  {group.repoName}
                </span>
                <span className="ml-auto shrink-0 rounded-full bg-card px-1.5 py-0.5 text-[10px] text-muted-foreground">
                  {group.matches.length}
                </span>
              </button>

              {/* Error indicator */}
              {group.error && (
                <div className="mx-2 mt-0.5 rounded border border-destructive/40 bg-destructive/10 px-2 py-1 text-[10px] text-destructive">
                  {group.error}
                </div>
              )}

              {/* Match rows grouped by file */}
              {!isCollapsed && group.matches.length > 0 && (
                <WorkspaceRepoMatchList
                  matches={group.matches}
                  query={query}
                  useRegex={useRegex}
                  caseSensitive={caseSensitive}
                  entryByPath={entryByPath}
                  onOpenFile={onOpenFile}
                />
              )}

              {group.truncated && !isCollapsed && (
                <div className="px-4 py-0.5 text-[10px] text-muted-foreground">
                  Results truncated for this repo.
                </div>
              )}
            </div>
          );
        })}
    </div>
  );
}

function WorkspaceRepoMatchList({
  matches,
  query,
  useRegex,
  caseSensitive,
  entryByPath,
  onOpenFile,
}: {
  matches: Match[];
  query: string;
  useRegex: boolean;
  caseSensitive: boolean;
  entryByPath: Map<string, DirEntry>;
  onOpenFile: (path: string, entry: DirEntry, line?: number) => void;
}) {
  // Group by file within this repo.
  const grouped = useMemo(() => {
    const by = new Map<string, Match[]>();
    for (const m of matches) {
      const arr = by.get(m.path);
      if (arr) arr.push(m);
      else by.set(m.path, [m]);
    }
    return Array.from(by, ([path, items]) => ({ path, items })).sort((a, b) =>
      a.path.localeCompare(b.path),
    );
  }, [matches]);

  return (
    <ul className="pl-4 text-[12px]">
      {grouped.map((g) => (
        <li key={g.path}>
          <div className="flex items-center gap-1 px-2 py-0.5 text-muted-foreground">
            <FileText size={10} className="shrink-0" />
            <span className="truncate font-mono text-[10.5px]">{g.path}</span>
            <span className="ml-auto shrink-0 text-[9px]">{g.items.length}</span>
          </div>
          <ul>
            {g.items.map((m, i) => (
              <li key={i}>
                <MatchRow
                  match={m}
                  query={query}
                  useRegex={useRegex}
                  caseSensitive={caseSensitive}
                  entryByPath={entryByPath}
                  onOpenFile={onOpenFile}
                />
              </li>
            ))}
          </ul>
        </li>
      ))}
    </ul>
  );
}

// ── Tree Results (grouped by file, collapsible) ─────────────────────

function TreeResults({
  grouped,
  query,
  useRegex,
  caseSensitive,
  showReplace,
  entryByPath,
  onOpenFile,
  onToggleCollapse,
  onReplaceInFile,
}: {
  grouped: FileGroup[];
  query: string;
  useRegex: boolean;
  caseSensitive: boolean;
  showReplace: boolean;
  entryByPath: Map<string, DirEntry>;
  onOpenFile: (path: string, entry: DirEntry, line?: number) => void;
  onToggleCollapse: (path: string) => void;
  onReplaceInFile: (path: string) => void;
}) {
  return (
    <ul className="py-1 text-[12px]" role="tree">
      {grouped.map((g) => (
        <li key={g.path} role="treeitem" aria-expanded={!g.collapsed}>
          {/* File header */}
          <div
            className="group flex items-center gap-1 px-2 py-0.5 hover:bg-accent"
          >
            <button
              type="button"
              onClick={() => onToggleCollapse(g.path)}
              className="shrink-0 rounded p-0.5 text-muted-foreground"
              aria-label={g.collapsed ? "Expand" : "Collapse"}
            >
              {g.collapsed ? <ChevronRight size={12} /> : <ChevronDown size={12} />}
            </button>
            <FileText size={12} className="shrink-0 text-muted-foreground" />
            <button
              type="button"
              onClick={() => {
                const entry = entryByPath.get(g.path);
                if (entry) onOpenFile(g.path, entry);
              }}
              className="min-w-0 flex-1 truncate text-left font-mono text-[11.5px] text-foreground hover:underline"
              title={g.path}
            >
              {g.path}
            </button>
            <span className="shrink-0 rounded-full bg-card px-1.5 py-0.5 text-[10px] text-muted-foreground">
              {g.items.length}
            </span>
            {showReplace && (
              <button
                type="button"
                onClick={() => onReplaceInFile(g.path)}
                className="shrink-0 rounded p-0.5 text-muted-foreground opacity-0 hover:bg-accent hover:text-foreground group-hover:opacity-100"
                title={`Replace all in ${g.path}`}
              >
                <Replace size={12} />
              </button>
            )}
          </div>

          {/* Match rows */}
          {!g.collapsed && (
            <FileMatchList
              items={g.items}
              query={query}
              useRegex={useRegex}
              caseSensitive={caseSensitive}
              entryByPath={entryByPath}
              onOpenFile={onOpenFile}
            />
          )}
        </li>
      ))}
    </ul>
  );
}

// ── List Results (flat, no grouping) ────────────────────────────────

function ListResults({
  matches,
  query,
  useRegex,
  caseSensitive,
  entryByPath,
  onOpenFile,
}: {
  matches: Match[];
  query: string;
  useRegex: boolean;
  caseSensitive: boolean;
  entryByPath: Map<string, DirEntry>;
  onOpenFile: (path: string, entry: DirEntry, line?: number) => void;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const virtualizer = useVirtualizer({
    count: matches.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => MATCH_ROW_HEIGHT,
    overscan: 12,
  });

  return (
    <div ref={scrollRef} className="h-full overflow-y-auto">
      <ul
        role="listbox"
        className="relative"
        style={{ height: virtualizer.getTotalSize() }}
      >
        {virtualizer.getVirtualItems().map((virtualItem) => {
          const m = matches[virtualItem.index];
          return (
            <li
              key={virtualItem.index}
              role="option"
              style={{
                position: "absolute",
                top: 0,
                left: 0,
                width: "100%",
                height: MATCH_ROW_HEIGHT,
                transform: `translateY(${virtualItem.start}px)`,
              }}
            >
              <MatchRow
                match={m}
                query={query}
                useRegex={useRegex}
                caseSensitive={caseSensitive}
                entryByPath={entryByPath}
                onOpenFile={onOpenFile}
                showPath
              />
            </li>
          );
        })}
      </ul>
    </div>
  );
}

// ── Per-file match list ─────────────────────────────────────────────

function FileMatchList({
  items,
  query,
  useRegex,
  caseSensitive,
  entryByPath,
  onOpenFile,
}: {
  items: Match[];
  query: string;
  useRegex: boolean;
  caseSensitive: boolean;
  entryByPath: Map<string, DirEntry>;
  onOpenFile: (path: string, entry: DirEntry, line?: number) => void;
}) {
  if (items.length < VIRTUALISE_THRESHOLD) {
    return (
      <ul role="listbox" aria-label={`Matches in ${items[0]?.path ?? "file"}`}>
        {items.map((m, i) => (
          <li key={i} role="option">
            <MatchRow
              match={m}
              query={query}
              useRegex={useRegex}
              caseSensitive={caseSensitive}
              entryByPath={entryByPath}
              onOpenFile={onOpenFile}
            />
          </li>
        ))}
      </ul>
    );
  }

  return (
    <VirtualisedMatchList
      items={items}
      query={query}
      useRegex={useRegex}
      caseSensitive={caseSensitive}
      entryByPath={entryByPath}
      onOpenFile={onOpenFile}
    />
  );
}

function VirtualisedMatchList({
  items,
  query,
  useRegex,
  caseSensitive,
  entryByPath,
  onOpenFile,
}: {
  items: Match[];
  query: string;
  useRegex: boolean;
  caseSensitive: boolean;
  entryByPath: Map<string, DirEntry>;
  onOpenFile: (path: string, entry: DirEntry, line?: number) => void;
}) {
  // For large per-file groups inside tree mode, we cap the visible
  // height and let this sub-section scroll independently so it doesn't
  // blow out the outer container. Only one scrollbar is visible because
  // the outer container uses overflow-auto and this is a contained region.
  const scrollRef = useRef<HTMLDivElement>(null);

  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => MATCH_ROW_HEIGHT,
    overscan: 8,
  });

  const maxVisibleRows = 20;
  const containerHeight = Math.min(
    items.length * MATCH_ROW_HEIGHT,
    maxVisibleRows * MATCH_ROW_HEIGHT,
  );

  return (
    <div
      ref={scrollRef}
      className="overflow-y-auto"
      style={{ height: containerHeight, contain: "strict" }}
    >
      <ul
        role="listbox"
        aria-label={`Matches in ${items[0]?.path ?? "file"}`}
        className="relative"
        style={{ height: virtualizer.getTotalSize() }}
      >
        {virtualizer.getVirtualItems().map((virtualItem) => {
          const m = items[virtualItem.index];
          return (
            <li
              key={virtualItem.index}
              role="option"
              style={{
                position: "absolute",
                top: 0,
                left: 0,
                width: "100%",
                height: MATCH_ROW_HEIGHT,
                transform: `translateY(${virtualItem.start}px)`,
              }}
            >
              <MatchRow
                match={m}
                query={query}
                useRegex={useRegex}
                caseSensitive={caseSensitive}
                entryByPath={entryByPath}
                onOpenFile={onOpenFile}
              />
            </li>
          );
        })}
      </ul>
    </div>
  );
}

// ── Match row with highlighted search term ──────────────────────────

function MatchRow({
  match: m,
  query,
  useRegex,
  caseSensitive,
  entryByPath,
  onOpenFile,
  showPath = false,
}: {
  match: Match;
  query: string;
  useRegex: boolean;
  caseSensitive: boolean;
  entryByPath: Map<string, DirEntry>;
  onOpenFile: (path: string, entry: DirEntry, line?: number) => void;
  showPath?: boolean;
}) {
  const highlighted = useMemo(
    () => highlightMatch(m.content, query, useRegex, caseSensitive),
    [m.content, query, useRegex, caseSensitive],
  );

  return (
    <button
      type="button"
      onClick={() => {
        const entry = entryByPath.get(m.path);
        if (entry) onOpenFile(m.path, entry, m.line);
      }}
      className={clsx(
        "flex w-full items-baseline gap-2 px-4 py-0.5 text-left font-mono text-[11.5px] text-muted-foreground hover:bg-accent",
      )}
      style={{ height: MATCH_ROW_HEIGHT }}
      title={showPath ? `${m.path}:${m.line}` : m.content}
    >
      {showPath && (
        <span className="shrink-0 max-w-[120px] truncate text-[10px] text-muted-foreground opacity-70">
          {m.path}
        </span>
      )}
      <span className="shrink-0 text-[10px] opacity-60">{m.line}</span>
      <span className="truncate text-foreground">{highlighted}</span>
    </button>
  );
}

// ── Highlight helper ────────────────────────────────────────────────

function highlightMatch(
  content: string,
  query: string,
  useRegex: boolean,
  caseSensitive: boolean,
): React.ReactNode {
  if (!query) return content;

  try {
    const flags = caseSensitive ? "g" : "gi";
    const pattern = useRegex ? query : escapeRegExp(query);
    const re = new RegExp(`(${pattern})`, flags);
    const parts = content.split(re);

    if (parts.length <= 1) return content;

    return parts.map((part, i) =>
      re.test(part) ? (
        <mark
          key={i}
          className="rounded-sm bg-primary/30 text-foreground"
        >
          {part}
        </mark>
      ) : (
        <span key={i}>{part}</span>
      ),
    );
  } catch {
    // If regex is invalid, just return plain text.
    return content;
  }
}

function escapeRegExp(str: string): string {
  return str.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

// ── Small UI components ─────────────────────────────────────────────

/** Toggle button inside the search/replace input fields (Aa, ab, .*) */
function ToggleButton({
  icon,
  title,
  active,
  onClick,
}: {
  icon: React.ReactNode;
  title: string;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={clsx(
        "shrink-0 rounded p-0.5 mx-0.5 transition-colors",
        active
          ? "bg-primary/20 text-primary"
          : "text-muted-foreground hover:bg-accent hover:text-foreground",
      )}
      title={title}
      aria-pressed={active}
    >
      {icon}
    </button>
  );
}

/** Toolbar button in the header area */
function ToolbarButton({
  icon,
  title,
  active,
  onClick,
}: {
  icon: React.ReactNode;
  title: string;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={clsx(
        "rounded p-1 transition-colors",
        active
          ? "bg-primary/20 text-primary"
          : "text-muted-foreground hover:bg-accent hover:text-foreground",
      )}
      title={title}
      aria-pressed={active}
    >
      {icon}
    </button>
  );
}
