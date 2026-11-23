import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { type Change, type Repository } from "@/lib/api";
import { useBrowserPreference } from "@/lib/browser-preference";
import { type CodeThemes } from "@/lib/code-theme";
import { useEffect, useMemo, useRef, useState } from "react";
import { DiffView } from "./file";
import type { DiffReviewState, DiffTarget } from "./model";
import { changePanelId } from "./model";
import { ChangeTree } from "./tree";

export function ChangeWorkspace({
  changes,
  repo,
  rev,
  base,
  theme,
  codeThemes,
  sticky,
  idPrefix = "",
  review,
}: {
  changes: Change[];
  repo: Pick<Repository, "owner" | "name">;
  rev: string;
  base?: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  sticky: boolean;
  idPrefix?: string;
  review?: DiffReviewState;
}) {
  const diffPane = useRef<HTMLDivElement>(null);
  const heading = `${idPrefix}changed-files-heading`;
  const [selectedPath, setSelectedPath] = useState<string>();
  const [filter, setFilter] = useState({ path: "", hideViewed: false });
  const [whitespace, setWhitespace] = useBrowserPreference(
    `crab-diff-whitespace-${review?.preferenceScope ?? "comparison"}`,
    ["show", "hide"],
  );
  const [dismissedTarget, setDismissedTarget] = useState<DiffTarget>();
  const target =
    review?.target === dismissedTarget ? undefined : review?.target;
  const revealedPath = target?.pathHex;
  const path = filter.path.trim().toLowerCase();
  const isViewed = filter.hideViewed ? review?.isViewed : undefined;
  const filtered = Boolean(path || isViewed);
  const visible = useMemo(() => {
    if (!filtered) return changes;
    return changes.filter(
      (file) =>
        file.path_hex === revealedPath ||
        (file.path.toLowerCase().includes(path) && !isViewed?.(file)),
    );
  }, [changes, filtered, isViewed, path, revealedPath]);
  const activePath = visible.some((file) => file.path === selectedPath)
    ? selectedPath
    : visible[0]?.path;
  const tracksScroll = review !== undefined;

  function updateFilter(path: string, hideViewed: boolean) {
    setFilter({ path, hideViewed });
    setDismissedTarget(review?.target);
    setSelectedPath(undefined);
    diffPane.current?.scrollTo({ top: 0 });
  }

  useEffect(() => {
    const pane = diffPane.current;
    if (!pane || !tracksScroll || visible.length < 2) return;
    let frame = 0;
    const updateSelection = () => {
      frame = 0;
      const paneTop = pane.getBoundingClientRect().top;
      // Panels are in file order. Find the last one at the top of the pane
      // without measuring every mounted panel each time the reviewer scrolls.
      let start = 0;
      let end = visible.length;
      while (start < end) {
        const middle = Math.floor((start + end) / 2);
        const panel = document.getElementById(
          idPrefix + changePanelId(visible[middle].path_hex),
        );
        if (panel && panel.getBoundingClientRect().top - paneTop <= 32) {
          start = middle + 1;
        } else {
          end = middle;
        }
      }
      setSelectedPath(visible[Math.max(0, start - 1)].path);
    };
    const onScroll = () => {
      if (frame) return;
      frame = requestAnimationFrame(updateSelection);
    };
    updateSelection();
    pane.addEventListener("scroll", onScroll, { passive: true });
    return () => {
      pane.removeEventListener("scroll", onScroll);
      if (frame) cancelAnimationFrame(frame);
    };
  }, [idPrefix, tracksScroll, visible]);

  function scrollToChange(change: Change) {
    const pane = diffPane.current;
    const panel = document.getElementById(
      idPrefix + changePanelId(change.path_hex),
    );
    if (!pane || !panel) return;
    setSelectedPath(change.path);
    const top =
      pane.scrollTop +
      panel.getBoundingClientRect().top -
      pane.getBoundingClientRect().top;
    pane.scrollTo({ top });
  }

  return (
    <>
      <h3 id={heading}>
        {changes.length} changed {changes.length === 1 ? "file" : "files"}
      </h3>
      {changes.length > 0 && (
        <div className="mb-3 flex flex-wrap items-center gap-x-4 gap-y-2">
          <Input
            type="search"
            aria-label="Filter changed files"
            placeholder="Filter by path"
            value={filter.path}
            className="sm:max-w-72"
            onChange={(event) =>
              updateFilter(event.target.value, filter.hideViewed)
            }
          />
          {review && (
            <label className="inline-flex items-center gap-2 text-sm">
              <Checkbox
                aria-label="Hide viewed"
                checked={filter.hideViewed}
                disabled={!review.isViewed}
                onCheckedChange={(checked) =>
                  updateFilter(filter.path, checked === true)
                }
              />
              Hide viewed
            </label>
          )}
          <label className="inline-flex items-center gap-2 text-sm">
            <Checkbox
              aria-label="Hide whitespace"
              checked={whitespace === "hide"}
              onCheckedChange={(checked) => {
                setDismissedTarget(review?.target);
                setWhitespace(checked === true ? "hide" : "show");
              }}
            />
            Hide whitespace
          </label>
          {(filter.path || filter.hideViewed) && (
            <Button
              size="sm"
              variant="ghost"
              onClick={() => updateFilter("", false)}
            >
              Clear filters
            </Button>
          )}
          {filtered && (
            <p className="text-sm text-muted-foreground" role="status">
              {visible.length} of {changes.length} files shown
            </p>
          )}
        </div>
      )}
      {visible.length ? (
        <section
          className={`change-workspace${sticky ? " change-workspace-sticky" : ""}`}
          aria-labelledby={heading}
        >
          <div className="change-tree-pane">
            <ChangeTree
              key={review?.fileSummaryVersion ?? "changed-files"}
              changes={visible}
              selected={activePath}
              onSelect={scrollToChange}
              review={review}
            />
          </div>
          <div
            ref={diffPane}
            className="change-diff-pane"
            aria-label="Changed file diffs"
          >
            <div className="change-diff-list">
              {visible.map((change, index) => (
                <DiffView
                  key={change.path_hex}
                  id={idPrefix + changePanelId(change.path_hex)}
                  repo={repo}
                  rev={rev}
                  base={base}
                  change={change}
                  theme={theme}
                  codeThemes={codeThemes}
                  eager={index === 0}
                  review={review}
                  hideWhitespace={whitespace === "hide"}
                  target={change.path_hex === revealedPath ? target : undefined}
                />
              ))}
            </div>
          </div>
        </section>
      ) : (
        <p className="muted">
          {changes.length
            ? "No changed files match these filters."
            : "This comparison has no changed files."}
        </p>
      )}
    </>
  );
}
