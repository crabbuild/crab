import { Button } from "@/components/ui/button";
import { Result } from "@/components/request/result";
import { Badge } from "@/components/ui/badge";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import {
  endpoint,
  useRequest,
  type Change,
  type Diff,
  type Repository,
} from "@/lib/api";
import {
  diffColors,
  enableCodeKeyboardScroll,
  type CodeThemes,
} from "@/lib/code-theme";
import {
  parseDiffFromFile,
  type DiffLineAnnotation,
  type SelectedLineRange,
} from "@pierre/diffs";
import { FileDiff } from "@pierre/diffs/react";
import { useEffect, useMemo, useRef, useState } from "react";
import type { DiffReviewState, DiffTarget, InlineDiffThread } from "./model";
import { diffIgnoringWhitespace } from "./whitespace";

function changeVariant(kind: string) {
  if (kind === "Added") return "success" as const;
  if (kind === "Deleted") return "destructive" as const;
  return "secondary" as const;
}

export function DiffView({
  id,
  repo,
  rev,
  base,
  change,
  theme,
  codeThemes,
  eager,
  review,
  target,
  hideWhitespace,
  mergedPull,
}: {
  id: string;
  repo: Pick<Repository, "owner" | "name">;
  rev: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  base?: string;
  change: Change;
  eager: boolean;
  review?: DiffReviewState;
  target?: DiffTarget;
  hideWhitespace: boolean;
  mergedPull?: number;
}) {
  const panel = useRef<HTMLElement>(null);
  const [load, setLoad] = useState(eager);
  useEffect(() => {
    const node = panel.current;
    if (!node || !target) return;
    setLoad(true);
    const previousFocus = document.activeElement;
    node.scrollIntoView({ block: "center" });
    const focusAnnotation = () => {
      const annotation = node.querySelector<HTMLButtonElement>(
        `[data-thread-number="${target.thread}"]`,
      );
      if (
        document.activeElement === previousFocus ||
        document.activeElement === document.body
      ) {
        annotation?.focus({ preventScroll: true });
      }
      return annotation !== null;
    };
    if (focusAnnotation()) return;
    // A revealed file may still be waiting for its pinned diff and annotation.
    const observer = new MutationObserver(() => {
      if (focusAnnotation()) observer.disconnect();
    });
    observer.observe(node, { childList: true, subtree: true });
    return () => observer.disconnect();
  }, [target]);
  useEffect(() => {
    const node = panel.current;
    if (load || !node) return;
    if (!("IntersectionObserver" in window)) {
      setLoad(true);
      return;
    }
    const root = node.closest(".change-diff-pane");
    const observer = new IntersectionObserver(
      (entries) => {
        if (!entries.some((entry) => entry.isIntersecting)) return;
        setLoad(true);
        observer.disconnect();
      },
      { root, rootMargin: "600px 0px" },
    );
    observer.observe(node);
    return () => observer.disconnect();
  }, [load]);
  const state = useRequest<Diff>(
    !load
      ? null
      : mergedPull === undefined
        ? endpoint(repo, "diff", { rev, base, path_hex: change.path_hex })
        : endpoint(repo, `pulls/${mergedPull}/merged-comparison/file`, {
            path_hex: change.path_hex,
          }),
  );
  const [style, setStyle] = useState<"unified" | "split">("split");
  const options = useMemo(
    () => ({
      theme: codeThemes,
      themeType: theme,
      diffStyle: style,
      preferredHighlighter: "shiki-js" as const,
      onPostRender: enableCodeKeyboardScroll,
      ...(review
        ? {
            enableLineSelection: true,
            onLineSelected: (range: SelectedLineRange | null) =>
              review.onLineSelected(change, range),
          }
        : {}),
    }),
    [change, codeThemes, review, style, theme],
  );
  const files = useMemo(() => {
    const data = state.data;
    if (
      !data ||
      (!data.old && !data.new) ||
      (data.old && data.old.text === null) ||
      (data.new && data.new.text === null)
    )
      return null;
    const oldFile = data.old
      ? {
          name: data.path,
          contents: data.old.text ?? "",
          cacheKey: data.old.oid,
        }
      : null;
    const newFile = data.new
      ? {
          name: data.path,
          contents: data.new.text ?? "",
          cacheKey: data.new.oid,
        }
      : null;
    if (newFile) return { oldFile, newFile };
    if (oldFile) return { oldFile, newFile: null };
    return null;
  }, [state.data]);
  const lineAnnotations = useMemo<DiffLineAnnotation<InlineDiffThread>[]>(
    () =>
      (review?.threads ?? [])
        .filter(
          (thread) =>
            thread.path_hex === change.path_hex &&
            !thread.outdated &&
            !thread.vanished,
        )
        .map((thread) => ({
          side: thread.side === "old" ? "deletions" : "additions",
          lineNumber: thread.start_line,
          metadata: thread,
        })),
    [change.path_hex, review?.threads],
  );
  const complete =
    !state.data?.old?.text_truncated && !state.data?.new?.text_truncated;
  const filteredDiff = useMemo(
    () =>
      hideWhitespace && complete && files?.oldFile && files.newFile
        ? diffIgnoringWhitespace(files.oldFile, files.newFile)
        : undefined,
    [complete, files, hideWhitespace],
  );
  const anchor =
    target &&
    lineAnnotations.find(({ metadata }) => metadata.number === target.thread);
  const revealWhitespace = !!(
    filteredDiff &&
    anchor &&
    !filteredDiff.hunks.some((hunk) => {
      const start =
        anchor.side === "additions" ? hunk.additionStart : hunk.deletionStart;
      const count =
        anchor.side === "additions" ? hunk.additionCount : hunk.deletionCount;
      return (
        anchor.metadata.start_line >= start &&
        anchor.metadata.end_line < start + count
      );
    })
  );
  const renderedDiff = useMemo(() => {
    if (filteredDiff && !revealWhitespace) return filteredDiff;
    return files ? parseDiffFromFile(files.oldFile, files.newFile) : undefined;
  }, [files, filteredDiff, revealWhitespace]);
  return (
    <section
      ref={panel}
      id={id}
      className="panel diff-panel"
      data-change-path={change.path}
      data-change-path-hex={change.path_hex}
      aria-labelledby={`${id}-heading`}
    >
      <div className="panel-header">
        <div className="diff-file-heading">
          <h4 id={`${id}-heading`}>{change.path}</h4>
          <Badge variant={changeVariant(change.kind)}>{change.kind}</Badge>
          {change.old?.mode !== change.new?.mode && (
            <code>
              {change.old?.mode ?? "—"} → {change.new?.mode ?? "—"}
            </code>
          )}
        </div>
        <div className="flex flex-wrap items-center justify-end gap-3">
          {review?.renderFileActions?.(change)}
          <ToggleGroup
            role="group"
            type="single"
            variant="outline"
            spacing={0}
            aria-label={`Diff layout for ${change.path}`}
            value={style}
            onValueChange={(value) => {
              if (value === "split" || value === "unified") setStyle(value);
            }}
          >
            <ToggleGroupItem
              value="split"
              role="button"
              aria-pressed={style === "split"}
              aria-checked={undefined}
            >
              Split
            </ToggleGroupItem>
            <ToggleGroupItem
              value="unified"
              role="button"
              aria-pressed={style === "unified"}
              aria-checked={undefined}
            >
              Unified
            </ToggleGroupItem>
          </ToggleGroup>
        </div>
      </div>
      {load ? (
        <Result state={state}>
          {() =>
            filteredDiff && !revealWhitespace && !filteredDiff.hunks.length ? (
              <div className="notice">
                No text changes with whitespace hidden.
              </div>
            ) : renderedDiff ? (
              <>
                {revealWhitespace && (
                  <p className="notice">
                    Showing whitespace for the selected review comment.
                  </p>
                )}
                <FileDiff<InlineDiffThread>
                  key={`${theme}:${codeThemes.light}:${codeThemes.dark}`}
                  fileDiff={renderedDiff}
                  options={options}
                  lineAnnotations={lineAnnotations}
                  renderAnnotation={(annotation) => {
                    const thread = annotation.metadata;
                    return (
                      <Button
                        variant="ghost"
                        type="button"
                        className="inline-review-annotation"
                        data-thread-number={thread.number}
                        aria-label={`Open review thread #${thread.number}`}
                        onClick={() => review?.onThreadSelected(thread)}
                      >
                        {review?.renderAnnotation?.(thread) ??
                          `Comment #${thread.number}`}
                      </Button>
                    );
                  }}
                  style={diffColors}
                />
              </>
            ) : (
              <div className="notice">
                Binary content changed. Browse the corresponding revision to
                download it.
              </div>
            )
          }
        </Result>
      ) : (
        <div className="diff-placeholder muted" aria-hidden="true">
          Diff loads as it comes into view.
        </div>
      )}
    </section>
  );
}
