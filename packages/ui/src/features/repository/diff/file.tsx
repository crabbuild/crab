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
import type { DiffLineAnnotation, SelectedLineRange } from "@pierre/diffs";
import { MultiFileDiff } from "@pierre/diffs/react";
import { useEffect, useMemo, useRef, useState } from "react";
import type { DiffReviewState, InlineDiffThread } from "./model";
import { changePanelId } from "./model";

function changeVariant(kind: string) {
  if (kind === "Added") return "success" as const;
  if (kind === "Deleted") return "destructive" as const;
  return "secondary" as const;
}

export function DiffView({
  repo,
  rev,
  base,
  change,
  theme,
  codeThemes,
  eager,
  review,
}: {
  repo: Repository;
  rev: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  base?: string;
  change: Change;
  eager: boolean;
  review?: DiffReviewState;
}) {
  const panel = useRef<HTMLElement>(null);
  const [load, setLoad] = useState(eager);
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
    load
      ? endpoint(repo, "diff", { rev, base, path_hex: change.path_hex })
      : null,
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
              review.onLineSelected(change.path_hex, range),
          }
        : {}),
    }),
    [change.path_hex, codeThemes, review, style, theme],
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
  return (
    <section
      ref={panel}
      id={changePanelId(change.path_hex)}
      className="panel diff-panel"
      data-change-path={change.path}
      data-change-path-hex={change.path_hex}
      aria-labelledby={`${changePanelId(change.path_hex)}-heading`}
    >
      <div className="panel-header">
        <div className="diff-file-heading">
          <h4 id={`${changePanelId(change.path_hex)}-heading`}>
            {change.path}
          </h4>
          <Badge variant={changeVariant(change.kind)}>{change.kind}</Badge>
          {change.old?.mode !== change.new?.mode && (
            <code>
              {change.old?.mode ?? "—"} → {change.new?.mode ?? "—"}
            </code>
          )}
        </div>
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
      {load ? (
        <Result state={state}>
          {() =>
            files ? (
              <MultiFileDiff<InlineDiffThread>
                key={`${theme}:${codeThemes.light}:${codeThemes.dark}`}
                {...files}
                options={options}
                lineAnnotations={lineAnnotations}
                renderAnnotation={(annotation) => {
                  const thread = annotation.metadata;
                  return (
                    <button
                      type="button"
                      className="inline-review-annotation"
                      data-thread-number={thread.number}
                      aria-label={`Open review thread #${thread.number}`}
                      onClick={() => review?.onThreadSelected(thread)}
                    >
                      {review?.renderAnnotation?.(thread) ??
                        `Comment #${thread.number}`}
                    </button>
                  );
                }}
                style={diffColors}
              />
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
