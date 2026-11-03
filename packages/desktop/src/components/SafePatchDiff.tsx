import { Component, type ErrorInfo, type ReactNode } from "react";
import { PatchDiff, type DiffLineAnnotation } from "@pierre/diffs/react";
import type { SelectedLineRange } from "@pierre/diffs";
import { FileCode2, Package } from "lucide-react";
import { isPointerPatch, pointerKindLabel } from "../lib/pointer-detection";
import { parsePatch, type ParsedFile, type ParsedHunk } from "../lib/parse-patch";

/**
 * Character-count threshold above which we proactively split the
 * patch into per-hunk renders instead of attempting a single-shot
 * PatchDiff. This avoids the flash-of-error from the error boundary
 * for known-large patches. ~50KB covers most normal files; anything
 * larger gets the chunked path up front.
 */
const LARGE_PATCH_THRESHOLD = 50_000;

interface Props {
  patch: string;
  className?: string;
  options?: Parameters<typeof PatchDiff>[0]["options"];
  /** Line annotations rendered inline in the diff at specific positions. */
  lineAnnotations?: DiffLineAnnotation<unknown>[];
  /** Render function for each annotation. */
  renderAnnotation?: (annotation: DiffLineAnnotation<unknown>) => ReactNode;
  /** Currently selected line range (controlled). */
  selectedLines?: SelectedLineRange | null;
}

interface State {
  error: Error | null;
}

/**
 * Error-boundary wrapper around `<PatchDiff />` from @pierre/diffs.
 *
 * Handles three cases:
 * 1. Crab/LFS pointer patches — detected up front and rendered as
 *    a purpose-built card showing pointer metadata instead of a
 *    broken unified diff.
 * 2. Binary diffs — shown as a simple info card.
 * 3. Large patches that crash PatchDiff — split into per-hunk
 *    sub-patches and rendered individually with PatchDiff, giving
 *    the same syntax-highlighted, line-numbered, word-level diff
 *    experience as smaller files.
 */
export class SafePatchDiff extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.warn("[SafePatchDiff] render error:", error.message, info.componentStack);
  }

  render(): ReactNode {
    const { patch, className, options, lineAnnotations, renderAnnotation, selectedLines } = this.props;

    // Detect pointer patches before attempting to render — these
    // will always fail in PatchDiff because the "diff" is just hex
    // pointer metadata, not a real unified patch.
    const detection = isPointerPatch(patch);
    if (detection.isPointer) {
      return <PointerDiffCard patch={patch} kind={detection.kind} />;
    }

    // Detect binary file diffs that git outputs as
    // "Binary files a/... and b/... differ"
    if (isBinaryDiff(patch)) {
      return <BinaryDiffCard patch={patch} />;
    }

    if (this.state.error) {
      // PatchDiff failed on the full patch — most commonly a large
      // text file that exceeds the renderer's internal limits. Split
      // into per-hunk sub-patches and render each individually with
      // PatchDiff. This gives us the same syntax highlighting, line
      // numbers, and word-level diffs as smaller files.
      return (
        <PerHunkFallback
          patch={this.props.patch}
          className={className}
          options={options}
          lineAnnotations={lineAnnotations}
          renderAnnotation={renderAnnotation}
          selectedLines={selectedLines}
        />
      );
    }

    // Proactively split large patches into per-hunk renders to avoid
    // hitting PatchDiff's internal limits. The threshold is generous
    // enough that most normal files render in one shot (fast path)
    // while truly large diffs get the chunked treatment up front.
    if (patch.length > LARGE_PATCH_THRESHOLD) {
      return (
        <PerHunkFallback
          patch={patch}
          className={className}
          options={options}
          lineAnnotations={lineAnnotations}
          renderAnnotation={renderAnnotation}
          selectedLines={selectedLines}
        />
      );
    }

    return (
      <PatchDiff
        patch={patch}
        className={className}
        options={options}
        lineAnnotations={lineAnnotations}
        renderAnnotation={renderAnnotation}
        selectedLines={selectedLines}
      />
    );
  }
}

// ── Per-hunk fallback (powered by @pierre/diffs) ─────────────────────

/**
 * When the full patch is too large for a single PatchDiff render,
 * split it into individual hunks and render each one separately.
 * This is the same strategy InlinePatchDiff uses for the Changes
 * view — each hunk gets its own `diff --git` + `---/+++` header
 * and a single `@@` block, which PatchDiff handles without issue.
 *
 * The result is identical to what diffs.software renders: syntax
 * highlighting, line numbers, word-level inline diffs, and hunk
 * separators — just chunked so no single render exceeds limits.
 */
function PerHunkFallback({
  patch,
  className,
  options,
  lineAnnotations,
  renderAnnotation,
  selectedLines,
}: {
  patch: string;
  className?: string;
  options?: Props["options"];
  lineAnnotations?: DiffLineAnnotation<unknown>[];
  renderAnnotation?: (annotation: DiffLineAnnotation<unknown>) => ReactNode;
  selectedLines?: SelectedLineRange | null;
}) {
  const parsed = parsePatch(patch);

  if (parsed.hunks.length === 0) {
    // Edge case: patch has no parseable hunks (mode-only change, or
    // the synthesized patch format doesn't match what the parser
    // expects). Try rendering with PatchDiff directly — it handles
    // more formats than our parser. Only show the "no changes" card
    // when there's genuinely no diff content.
    const path = extractPath(patch);
    const hasContent = patch.trim().split("\n").some(
      (l) => l.startsWith("+") || l.startsWith("-"),
    );
    if (hasContent) {
      // Attempt PatchDiff directly — it may handle formats our
      // parser doesn't recognise. Wrap in a per-hunk error boundary
      // so a crash still degrades gracefully.
      return (
        <HunkErrorBoundary fallbackPatch={patch}>
          <PatchDiff
            patch={patch}
            className={className}
            options={options}
            lineAnnotations={lineAnnotations}
            renderAnnotation={renderAnnotation}
            selectedLines={selectedLines}
          />
        </HunkErrorBoundary>
      );
    }
    return (
      <div className="flex items-center justify-center gap-2 py-6 text-sm text-gx-muted">
        <FileCode2 size={14} />
        <span>
          {path ? `${path} — ` : ""}No displayable changes
        </span>
      </div>
    );
  }

  return (
    <div className="flex flex-col">
      {parsed.files.map((file) => (
        <FileHunks
          key={file.path}
          file={file}
          className={className}
          options={options}
          lineAnnotations={lineAnnotations}
          renderAnnotation={renderAnnotation}
          selectedLines={selectedLines}
        />
      ))}
    </div>
  );
}

/**
 * Render all hunks for a single file, each as its own PatchDiff.
 * Wrapped in an individual error boundary so one bad hunk doesn't
 * take down the entire file.
 */
function FileHunks({
  file,
  className,
  options,
  lineAnnotations,
  renderAnnotation,
  selectedLines,
}: {
  file: ParsedFile;
  className?: string;
  options?: Props["options"];
  lineAnnotations?: DiffLineAnnotation<unknown>[];
  renderAnnotation?: (annotation: DiffLineAnnotation<unknown>) => ReactNode;
  selectedLines?: SelectedLineRange | null;
}) {
  const fileHeader = buildFileHeader(file.oldPath, file.path);

  return (
    <>
      {file.hunks.map((hunk) => {
        const subPatch = buildHunkPatch(fileHeader, hunk);
        return (
          <HunkErrorBoundary key={`${hunk.oldStart}-${hunk.newStart}-${hunk.indexInFile}`}>
            <PatchDiff
              patch={subPatch}
              className={className}
              options={options}
              lineAnnotations={lineAnnotations}
              renderAnnotation={renderAnnotation}
              selectedLines={selectedLines}
            />
          </HunkErrorBoundary>
        );
      })}
    </>
  );
}

/**
 * Minimal error boundary per hunk — if a single hunk still fails
 * to render (extremely rare), show a compact raw-text fallback for
 * just that hunk rather than crashing the whole file.
 */
class HunkErrorBoundary extends Component<
  { children: ReactNode; fallbackPatch?: string },
  { error: Error | null }
> {
  state = { error: null as Error | null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  render() {
    if (this.state.error) {
      if (this.props.fallbackPatch) {
        return (
          <pre className="whitespace-pre-wrap font-mono text-[10px] text-gx-text/70 px-2.5 py-2 max-h-[320px] overflow-auto">
            {this.props.fallbackPatch}
          </pre>
        );
      }
      return (
        <div className="border-b border-gx-border/30 bg-gx-bg-sub px-4 py-2 text-[11px] text-gx-muted">
          <span className="text-gx-warn">⚠</span> Hunk render failed — raw content shown below
        </div>
      );
    }
    return this.props.children;
  }
}

// ── Helpers ──────────────────────────────────────────────────────────

/** Check if the patch is a binary file diff that git can't show as text. */
function isBinaryDiff(patch: string): boolean {
  const lower = patch.toLowerCase();
  return (
    lower.includes("binary files") && lower.includes("differ")
  ) || (
    lower.includes("git binary patch")
  );
}

/** Extract the file path from a diff header like "diff --git a/path b/path". */
function extractPath(patch: string): string | null {
  const match = patch.match(/^diff --git a\/(.+?) b\/(.+?)$/m);
  return match ? match[2] : null;
}

/**
 * Build the `diff --git` + `--- a/` + `+++ b/` preamble that
 * PatchDiff expects before any `@@` header.
 */
function buildFileHeader(oldPath: string, newPath: string): string {
  return [
    `diff --git a/${oldPath} b/${newPath}`,
    `--- a/${oldPath}`,
    `+++ b/${newPath}`,
    "",
  ].join("\n");
}

/**
 * Assemble a single-hunk patch string from the file header + hunk.
 * PatchDiff handles this cleanly regardless of hunk size.
 */
function buildHunkPatch(fileHeader: string, hunk: ParsedHunk): string {
  const oldPart = hunk.oldLines === 1 ? String(hunk.oldStart) : `${hunk.oldStart},${hunk.oldLines}`;
  const newPart = hunk.newLines === 1 ? String(hunk.newStart) : `${hunk.newStart},${hunk.newLines}`;
  const hunkHdr = `@@ -${oldPart} +${newPart} @@`;
  return `${fileHeader}${hunkHdr}\n${hunk.body}`;
}

/** Parse pointer lines from the patch body (added/removed lines). */
function parsePointerLines(patch: string): { added: string[]; removed: string[] } {
  const added: string[] = [];
  const removed: string[] = [];
  for (const line of patch.split("\n")) {
    if (line.startsWith("@@")) continue;
    if (line.startsWith("diff ") || line.startsWith("index ") || line.startsWith("--- ") || line.startsWith("+++ ")) continue;
    if (line.startsWith("+") && line.length > 1) added.push(line.slice(1));
    else if (line.startsWith("-") && line.length > 1) removed.push(line.slice(1));
  }
  return { added, removed };
}

// ── Pointer & Binary cards ───────────────────────────────────────────

/** Purpose-built card for crab/LFS pointer file diffs. */
function PointerDiffCard({ patch, kind }: { patch: string; kind: "crab" | "lfs" | null }) {
  const path = extractPath(patch);
  const label = pointerKindLabel(kind);
  const { added, removed } = parsePointerLines(patch);
  const isNew = removed.length === 0 && added.length > 0;
  const isDeleted = added.length === 0 && removed.length > 0;
  const isModified = added.length > 0 && removed.length > 0;

  return (
    <div className="flex flex-col gap-3 p-5">
      {/* Header */}
      <div className="flex items-start gap-3">
        <div className="shrink-0 rounded-lg bg-gx-accent/10 p-2.5">
          <Package size={20} className="text-gx-accent" />
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="text-sm font-medium text-gx-text">{label}</span>
            <span className="rounded border border-gx-border px-1.5 py-0.5 text-[10px] text-gx-muted">
              {isNew ? "Added" : isDeleted ? "Removed" : isModified ? "Modified" : "Changed"}
            </span>
          </div>
          {path && (
            <div className="mt-1 truncate font-mono text-[11px] text-gx-muted" title={path}>
              {path}
            </div>
          )}
          <p className="mt-2 text-[11px] leading-relaxed text-gx-muted">
            This file is tracked by {kind === "lfs" ? "Git LFS" : "Crab"} as a content-addressed pointer.
            The actual file content is stored in cloud object storage — only the pointer metadata is committed to git.
          </p>
        </div>
      </div>

      {/* Pointer metadata comparison */}
      <div className="rounded-lg border border-gx-border bg-gx-bg-sub">
        {removed.length > 0 && (
          <div className="border-b border-gx-border/50 px-3 py-2">
            <div className="mb-1 text-[10px] font-semibold uppercase tracking-wider text-gx-error/80">
              {isDeleted ? "Removed Pointer" : "Previous Version"}
            </div>
            <div className="space-y-0.5 font-mono text-[11px]">
              {removed.slice(0, 8).map((line, i) => (
                <div key={i} className="truncate text-gx-muted">
                  <span className="mr-2 inline-block w-3 text-center text-gx-error">−</span>
                  {line}
                </div>
              ))}
              {removed.length > 8 && (
                <div className="text-[10px] text-gx-muted">… {removed.length - 8} more lines</div>
              )}
            </div>
          </div>
        )}
        {added.length > 0 && (
          <div className="px-3 py-2">
            <div className="mb-1 text-[10px] font-semibold uppercase tracking-wider text-gx-success/80">
              {isNew ? "New Pointer" : "New Version"}
            </div>
            <div className="space-y-0.5 font-mono text-[11px]">
              {added.slice(0, 8).map((line, i) => (
                <div key={i} className="truncate text-gx-text">
                  <span className="mr-2 inline-block w-3 text-center text-gx-success">+</span>
                  {line}
                </div>
              ))}
              {added.length > 8 && (
                <div className="text-[10px] text-gx-muted">… {added.length - 8} more lines</div>
              )}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

/** Card for binary file diffs that git can't render as text. */
function BinaryDiffCard({ patch }: { patch: string }) {
  const path = extractPath(patch);

  return (
    <div className="flex flex-col items-center justify-center gap-3 py-8">
      <div className="rounded-lg bg-gx-surface p-3">
        <FileCode2 size={24} className="text-gx-muted" />
      </div>
      <div className="text-center">
        <div className="text-sm text-gx-text">Binary file</div>
        {path && (
          <div className="mt-0.5 font-mono text-[11px] text-gx-muted">{path}</div>
        )}
        <p className="mt-2 max-w-xs text-[11px] text-gx-muted">
          This file is binary and cannot be displayed as a text diff.
        </p>
      </div>
    </div>
  );
}
