import { type Change, type Repository } from "@/lib/api";
import { type CodeThemes } from "@/lib/code-theme";
import { useEffect, useRef, useState } from "react";
import { DiffView } from "./file";
import type { DiffReviewState } from "./model";
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
  review,
}: {
  changes: Change[];
  repo: Repository;
  rev: string;
  base?: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  sticky: boolean;
  review?: DiffReviewState;
}) {
  const diffPane = useRef<HTMLDivElement>(null);
  const [selectedPath, setSelectedPath] = useState(changes[0]?.path);

  useEffect(() => {
    setSelectedPath(changes[0]?.path);
  }, [changes]);

  useEffect(() => {
    const pane = diffPane.current;
    const firstChange = changes[0];
    if (!pane || !review || !firstChange || changes.length < 2) return;
    let frame = 0;
    const updateSelection = () => {
      frame = 0;
      const paneTop = pane.getBoundingClientRect().top;
      let active = firstChange;
      for (const change of changes) {
        const panel = document.getElementById(changePanelId(change.path_hex));
        if (!panel) continue;
        if (panel.getBoundingClientRect().top - paneTop <= 32) active = change;
      }
      setSelectedPath((current) =>
        current === active.path ? current : active.path,
      );
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
  }, [changes]);

  function scrollToChange(change: Change) {
    const pane = diffPane.current;
    const panel = document.getElementById(changePanelId(change.path_hex));
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
      <h3 id="changed-files-heading">
        {changes.length} changed {changes.length === 1 ? "file" : "files"}
      </h3>
      {changes.length ? (
        <section
          className={`change-workspace${sticky ? " change-workspace-sticky" : ""}`}
          aria-labelledby="changed-files-heading"
        >
          <div className="change-tree-pane">
            <ChangeTree
              key={review?.fileSummaryVersion ?? "changed-files"}
              changes={changes}
              selected={selectedPath}
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
              {changes.map((change, index) => (
                <DiffView
                  key={change.path_hex}
                  repo={repo}
                  rev={rev}
                  base={base}
                  change={change}
                  theme={theme}
                  codeThemes={codeThemes}
                  eager={index === 0}
                  review={review}
                />
              ))}
            </div>
          </div>
        </section>
      ) : (
        <p className="muted">This comparison has no changed files.</p>
      )}
    </>
  );
}
