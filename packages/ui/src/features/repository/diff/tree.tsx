import { compareFileItems } from "@/features/repository/tree/sort";
import { type Change } from "@/lib/api";
import type { FileTreeRowDecorationContext, GitStatus } from "@pierre/trees";
import { FileTree, useFileTree } from "@pierre/trees/react";
import { useEffect, useMemo, useRef } from "react";
import type { DiffReviewState } from "./model";

function changeStatus(kind: string): GitStatus {
  if (kind === "Added") return "added";
  if (kind === "Deleted") return "deleted";
  return "modified";
}

export function ChangeTree({
  changes,
  selected,
  onSelect,
  review,
}: {
  changes: Change[];
  selected?: string;
  onSelect: (change: Change) => void;
  review?: DiffReviewState;
}) {
  const paths = useMemo(
    () => new Map(changes.map((change) => [change.path, change])),
    [changes],
  );
  const pathsRef = useRef(paths);
  pathsRef.current = paths;
  const select = useRef(onSelect);
  select.current = onSelect;
  const fileSummary = review?.fileSummary;
  const fileSummaries = useMemo(
    () =>
      new Map(
        changes.map((change) => [
          change.path_hex,
          fileSummary?.(change.path_hex) ?? null,
        ]),
      ),
    [changes, fileSummary],
  );
  const fileSummariesRef = useRef(fileSummaries);
  fileSummariesRef.current = fileSummaries;
  const syncingSelection = useRef(false);
  const renderRowDecoration = useRef(
    ({ item }: FileTreeRowDecorationContext) => {
      if (item.kind !== "file") return null;
      const change = pathsRef.current.get(item.path);
      const stats = fileSummariesRef.current.get(change?.path_hex ?? "");
      if (!stats || stats.total === 0) return null;
      const label = `${stats.total} ${stats.total === 1 ? "thread" : "threads"}`;
      return {
        text: label,
        title: `${label}; ${stats.open} open, ${stats.resolved} resolved${stats.outdated ? `, ${stats.outdated} outdated` : ""}`,
      };
    },
  ).current;
  const { model } = useFileTree({
    paths: changes.map((change) => change.path),
    gitStatus: changes.map((change) => ({
      path: change.path,
      status: changeStatus(change.kind),
    })),
    initialExpansion: "open",
    initialSelectedPaths: selected ? [selected] : [],
    flattenEmptyDirectories: false,
    density: "default",
    itemHeight: 32,
    icons: "complete",
    renderRowDecoration,
    unsafeCSS: `
      [data-item-section="decoration"] {
        color: var(--trees-fg);
        font-size: 11px;
        font-variant-numeric: tabular-nums;
        font-weight: 600;
      }
    `,
    renaming: false,
    dragAndDrop: false,
    sort: (left, right) =>
      compareFileItems(
        left.basename,
        left.isDirectory,
        right.basename,
        right.isDirectory,
      ),
    onSelectionChange(selectedPaths) {
      if (syncingSelection.current) return;
      const change = paths.get(selectedPaths[0] ?? "");
      if (change) select.current(change);
    },
  });
  useEffect(() => {
    if (!selected || model.getSelectedPaths()[0] === selected) return;
    syncingSelection.current = true;
    for (const path of model.getSelectedPaths()) {
      if (path !== selected) model.getItem(path)?.deselect();
    }
    model.getItem(selected)?.select();
    syncingSelection.current = false;
  }, [model, selected]);
  return (
    <div className="change-tree-shell">
      <header className="change-tree-header">
        <div>
          <span className="change-tree-kicker">
            {review ? "Review files" : "Changed files"}
          </span>
          <strong>
            {changes.length} {changes.length === 1 ? "file" : "files"}
          </strong>
        </div>
        {review && (
          <span className="change-tree-hint">Select a file to jump</span>
        )}
      </header>
      <FileTree
        model={model}
        className="change-file-tree"
        aria-label="Changed files"
      />
    </div>
  );
}
