import { Component, type ErrorInfo, type ReactNode } from "react";
import { PatchDiff, type DiffLineAnnotation } from "@pierre/diffs/react";
import type { SelectedLineRange } from "@pierre/diffs";
import { FileCode2 } from "lucide-react";
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
 * Preserves Git patches and binary summaries without inferring storage from
 * their text. Captured source comparisons own Crab/LFS classification.
 * Large patches render per hunk; failed renderers retain the raw patch.
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
    // Mode-only or unfamiliar Git output must remain visible. An unparsed
    // nonempty patch is not evidence that the file has no changes.
    return patch.trim().length === 0 ? (
      <div className="py-6 text-center text-sm text-muted-foreground">No changes.</div>
    ) : (
      <HunkErrorBoundary fallbackPatch={patch}>
        <PatchDiff patch={patch} className={className} options={options}
          lineAnnotations={lineAnnotations} renderAnnotation={renderAnnotation} selectedLines={selectedLines} />
      </HunkErrorBoundary>
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
          <pre className="whitespace-pre-wrap font-mono text-[10px] text-foreground/70 px-2.5 py-2 max-h-[320px] overflow-auto">
            {this.props.fallbackPatch}
          </pre>
        );
      }
      return (
        <div className="border-b border-border/30 bg-background px-4 py-2 text-[11px] text-muted-foreground">
          <span className="text-warning">⚠</span> Hunk render failed — raw content shown below
        </div>
      );
    }
    return this.props.children;
  }
}

// ── Helpers ──────────────────────────────────────────────────────────

/** Check if the patch is a binary file diff that git can't show as text. */
function isBinaryDiff(patch: string): boolean {
  return patch.split("\n").some((line) =>
    /^Binary files .+ and .+ differ$/.test(line) || line === "GIT binary patch",
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

/** Card for binary file diffs that git can't render as text. */
function BinaryDiffCard({ patch }: { patch: string }) {
  const path = extractPath(patch);

  return (
    <div className="flex flex-col items-center justify-center gap-3 py-8">
      <div className="rounded-lg bg-card p-3">
        <FileCode2 size={24} className="text-muted-foreground" />
      </div>
      <div className="text-center">
        <div className="text-sm text-foreground">Binary file</div>
        {path && (
          <div className="mt-0.5 font-mono text-[11px] text-muted-foreground">{path}</div>
        )}
        <p className="mt-2 max-w-xs text-[11px] text-muted-foreground">
          This file is binary and cannot be displayed as a text diff.
        </p>
      </div>
    </div>
  );
}
