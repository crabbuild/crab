import { Fragment, useMemo } from "react";
import { PatchDiff } from "@pierre/diffs/react";
import { Trash2 } from "lucide-react";
import { HunkToolbar } from "./HunkToolbar";
import { PointerFileCard } from "./PointerFileCard";
import { diffOptions } from "../lib/diffTheme";
import { useEffectiveTheme } from "../hooks/useEffectiveTheme";
import { parsePatch, ParsedFile, ParsedHunk } from "../lib/parse-patch";
import { isPointerPatch, PointerPatchKind } from "../lib/pointer-detection";

export interface InlinePatchDiffAction {
  hunk: ParsedHunk;
}

export interface InlinePatchDiffLineAction {
  hunk: ParsedHunk;
  side: "add" | "del";
  /** Absolute line number in the respective (old/new) side. */
  absoluteLine: number;
  /** Raw body line (with `+` / `-` prefix). */
  rawLine: string;
}

/** Shape passed to the whole-file pointer callbacks. `kind` is the
 *  detected pointer flavour so consumers can branch on it if they
 *  need to (e.g. policy that treats LFS differently). `hunks` is
 *  every hunk parsed for this file — consumers that don't supply
 *  a dedicated `onStageFile` callback fall back to looping the
 *  per-hunk callbacks over this list. */
export interface InlinePatchDiffFileAction {
  path: string;
  kind: PointerPatchKind | null;
  hunks: ParsedHunk[];
}

export interface InlinePatchDiffProps {
  /** Unified patch string, e.g. the output of `git diff` for one
   *  file or for multiple files. Each hunk gets its own toolbar —
   *  except pointer-file blocks, which collapse to a single
   *  whole-file card (see `PointerFileCard`). */
  patch: string;
  canUnstage?: boolean;
  onStageHunk: (action: InlinePatchDiffAction) => void;
  onUnstageHunk?: (action: InlinePatchDiffAction) => void;
  onDiscardHunk: (action: InlinePatchDiffAction) => void;
  onDiscardLine?: (action: InlinePatchDiffLineAction) => void;
  /** Whole-file owners handle pointers and binary/mode changes. Discard has
   * no text-hunk fallback: reconstruction and confirmation belong to its owner. */
  onStageFile?: (action: InlinePatchDiffFileAction) => void;
  onUnstageFile?: (action: InlinePatchDiffFileAction) => void;
  onDiscardFile?: (action: InlinePatchDiffFileAction) => void;
  /** Rendered inside the right pane's scroll container when there
   *  are no hunks (e.g. the user cleared all edits). */
  emptyState?: React.ReactNode;
}

/**
 * `<PatchDiff />` augmented with a per-hunk toolbar on top and a
 * minimal per-line discard affordance inline.
 *
 * Strategy: parse the patch once, then render one `<PatchDiff />`
 * per hunk. Each sub-patch carries the same `diff --git` /
 * `--- a/` / `+++ b/` header plus a single `@@` hunk, which gives
 * `@pierre/diffs` exactly what it expects and eliminates any need
 * to pixel-align an absolute-positioned overlay against the
 * shadow-dom contents.
 *
 * Pointer detection: before rendering a file's hunks, we rebuild
 * the file-scoped patch string and run `isPointerPatch` against
 * it. A positive match switches the affordance for that file to a
 * single `<PointerFileCard />` — the design doc calls for whole-
 * file staging on pointer blobs because hex-aware per-hunk staging
 * is explicitly out of scope for v1. We still render the read-only
 * `<PatchDiff />` below the card so the user can see exactly what
 * will be staged; only the controls change.
 *
 * The per-line discard is rendered as a compact row list below
 * each hunk (only the `+`/`-` lines) — this keeps it decoupled
 * from the PatchDiff's internal DOM.
 */
export function InlinePatchDiff({
  patch,
  canUnstage = false,
  onStageHunk,
  onUnstageHunk,
  onDiscardHunk,
  onDiscardLine,
  onStageFile,
  onUnstageFile,
  onDiscardFile,
  emptyState,
}: InlinePatchDiffProps) {
  const parsed = useMemo(() => parsePatch(patch), [patch]);
  const effective = useEffectiveTheme();
  const options = useMemo(() => diffOptions(effective), [effective]);

  if (parsed.files.length === 0) {
    return (
      <div className="p-6 text-center text-sm text-muted-foreground">
        {emptyState ?? "No hunks in this patch."}
      </div>
    );
  }

  // Group hunks by file so we can render a file header between
  // per-hunk sections.
  return (
    <div className="flex flex-col">
      {parsed.files.map((f) => {
        const fileHeader = buildFileHeader(f.oldPath, f.path);
        const fileSubPatch = buildFileSubPatch(f, fileHeader);
        const pointer = isPointerPatch(fileSubPatch);
        return (
          <Fragment key={f.path}>
            <div className="sticky top-0 z-10 flex items-center gap-2 border-b border-border bg-card px-3 py-1.5 text-[11px]">
              <span className="font-mono text-foreground">{f.path}</span>
              {f.oldPath !== f.path && (
                <span className="text-muted-foreground">
                  (renamed from {f.oldPath})
                </span>
              )}
              <span className="ml-auto text-muted-foreground">
                {pointer.isPointer
                  ? "pointer file"
                  : `${f.hunks.length} hunk${f.hunks.length === 1 ? "" : "s"}`}
              </span>
            </div>
            {pointer.isPointer || f.hunks.length === 0 ? (
              <PointerFileBlock
                file={f}
                kind={pointer.kind}
                fileHeader={fileHeader}
                canUnstage={canUnstage}
                options={options}
                onStageFile={() =>
                  invokeFileAction(
                    f,
                    pointer.kind,
                    onStageFile,
                    (hunk) => onStageHunk({ hunk }),
                  )
                }
                onUnstageFile={
                  onUnstageHunk || onUnstageFile
                    ? () =>
                        invokeFileAction(
                          f,
                          pointer.kind,
                          onUnstageFile,
                          onUnstageHunk
                            ? (hunk) => onUnstageHunk({ hunk })
                            : undefined,
                        )
                    : undefined
                }
                onDiscardFile={onDiscardFile ? () => onDiscardFile({ path: f.path, kind: pointer.kind, hunks: f.hunks }) : undefined}
              />
            ) : (
              f.hunks.map((hunk) => (
                <HunkBlock
                  key={`${hunk.oldStart}-${hunk.newStart}-${hunk.indexInFile}`}
                  hunk={hunk}
                  fileHeader={fileHeader}
                  hunkNumber={hunk.indexInFile + 1}
                  hunkCount={f.hunks.length}
                  canUnstage={canUnstage}
                  options={options}
                  onStage={() => onStageHunk({ hunk })}
                  onUnstage={onUnstageHunk ? () => onUnstageHunk({ hunk }) : undefined}
                  onDiscard={() => onDiscardHunk({ hunk })}
                  onDiscardLine={onDiscardLine}
                />
              ))
            )}
          </Fragment>
        );
      })}
    </div>
  );
}

interface PointerFileBlockProps {
  file: ParsedFile;
  kind: PointerPatchKind | null;
  fileHeader: string;
  canUnstage: boolean;
  options: ReturnType<typeof diffOptions>;
  onStageFile: () => void;
  onUnstageFile?: () => void;
  onDiscardFile?: () => void;
}

/**
 * Pointer-file block: the `<PointerFileCard />` controls up top,
 * then a read-only render of every hunk so users can verify what
 * they're about to stage. We intentionally drop the per-hunk
 * toolbars and the per-line discard strip here — the entire point
 * of this affordance is that hex-aware partial editing isn't
 * supported. Showing the diff keeps the signal-to-noise high
 * (pointer diffs are tiny anyway).
 */
function PointerFileBlock({
  file,
  kind,
  fileHeader,
  canUnstage,
  options,
  onStageFile,
  onUnstageFile,
  onDiscardFile,
}: PointerFileBlockProps) {
  return (
    <div className="border-b border-border">
      <PointerFileCard
        path={file.path}
        kind={kind}
        canUnstage={canUnstage}
        onStageFile={onStageFile}
        onUnstageFile={onUnstageFile}
        onDiscardFile={onDiscardFile}
      />
      <div className="bg-background">
        {file.hunks.map((hunk) => (
          <PatchDiff
            key={`${hunk.oldStart}-${hunk.newStart}-${hunk.indexInFile}`}
            patch={`${fileHeader}${hunkHeader(hunk)}\n${hunk.body}`}
            options={options}
            className="gx-patch-diff"
          />
        ))}
      </div>
    </div>
  );
}

/**
 * Fire the whole-file callback if provided, else fan the per-hunk
 * callback out across every hunk for the file. Provided for the
 * pointer-card path only — the agent's staging verbs take an
 * array of anchors, so a dedicated `onStageFile` that passes all
 * hunks in one call is the efficient shape.
 *
 * The fallback exists so consumers of `<InlinePatchDiff />` that
 * haven't been updated to the new file-level callbacks keep
 * working. It's suboptimal (N RPCs where 1 would do) but correct:
 * the agent treats each call as an independent anchor set.
 */
function invokeFileAction(
  file: ParsedFile,
  kind: PointerPatchKind | null,
  fileCallback:
    | ((action: InlinePatchDiffFileAction) => void)
    | undefined,
  perHunkFallback?: (hunk: ParsedHunk) => void,
): void {
  if (fileCallback) {
    fileCallback({ path: file.path, kind, hunks: file.hunks });
    return;
  }
  if (!perHunkFallback) return;
  for (const hunk of file.hunks) {
    perHunkFallback(hunk);
  }
}

interface HunkBlockProps {
  hunk: ParsedHunk;
  fileHeader: string;
  hunkNumber: number;
  hunkCount: number;
  canUnstage: boolean;
  options: ReturnType<typeof diffOptions>;
  onStage: () => void;
  onUnstage?: () => void;
  onDiscard: () => void;
  onDiscardLine?: (action: InlinePatchDiffLineAction) => void;
}

function HunkBlock({
  hunk,
  fileHeader,
  hunkNumber,
  hunkCount,
  canUnstage,
  options,
  onStage,
  onUnstage,
  onDiscard,
  onDiscardLine,
}: HunkBlockProps) {
  // Assemble a minimal one-hunk patch for PatchDiff. The `@@`
  // header has to match the shape git originally emitted — `@@ -x
  // +y @@` when the count is 1, else `@@ -x,c +y,c @@`.
  const subPatch = useMemo(() => {
    const hdr = hunkHeader(hunk);
    return `${fileHeader}${hdr}\n${hunk.body}`;
  }, [hunk, fileHeader]);

  const header = hunkHeader(hunk);

  return (
    <div className="border-b border-border">
      <HunkToolbar
        hunkNumber={hunkNumber}
        hunkCount={hunkCount}
        header={header}
        canUnstage={canUnstage}
        onStage={onStage}
        onUnstage={onUnstage}
        onDiscard={onDiscard}
      />
      <div className="bg-background">
        <PatchDiff
          patch={subPatch}
          options={options}
          className="gx-patch-diff"
        />
      </div>
      {onDiscardLine && <LineDiscardStrip hunk={hunk} onDiscardLine={onDiscardLine} />}
    </div>
  );
}

/**
 * Per-line discard affordance. Lists every `+`/`-` line in the hunk
 * with a hover-revealed trash icon. Deliberately lightweight —
 * the full per-line staging UX is out of scope for task 1.2 and a
 * dedicated line-level discard is enough to deliver on the
 * requirement text ("per-hunk / per-line toolbar").
 */
function LineDiscardStrip({
  hunk,
  onDiscardLine,
}: {
  hunk: ParsedHunk;
  onDiscardLine: (action: InlinePatchDiffLineAction) => void;
}) {
  const lines = useMemo(() => {
    const out: Array<{
      side: "add" | "del";
      absoluteLine: number;
      raw: string;
    }> = [];
    const bodyLines = hunk.body.split("\n");
    if (bodyLines.length > 0 && bodyLines[bodyLines.length - 1] === "") {
      bodyLines.pop();
    }
    let oldCursor = hunk.oldStart;
    let newCursor = hunk.newStart;
    for (const bl of bodyLines) {
      if (bl.startsWith("\\ ")) continue;
      const prefix = bl[0];
      if (prefix === "+") {
        out.push({ side: "add", absoluteLine: newCursor, raw: bl });
        newCursor += 1;
      } else if (prefix === "-") {
        out.push({ side: "del", absoluteLine: oldCursor, raw: bl });
        oldCursor += 1;
      } else if (prefix === " ") {
        oldCursor += 1;
        newCursor += 1;
      }
    }
    return out;
  }, [hunk]);

  if (lines.length === 0) return null;

  return (
    <details className="group border-t border-border/50 bg-background text-[11px]">
      <summary className="cursor-pointer px-3 py-1 text-muted-foreground hover:text-foreground">
        Discard individual line{lines.length === 1 ? "" : "s"} ({lines.length})
      </summary>
      <ul className="divide-y divide-border/30">
        {lines.map((l, i) => (
          <li
            key={`${l.side}-${l.absoluteLine}-${i}`}
            className="group/row flex items-center gap-2 px-3 py-0.5 font-mono text-[11px] hover:bg-accent"
          >
            <span
              className={
                l.side === "add"
                  ? "w-10 shrink-0 text-right text-success"
                  : "w-10 shrink-0 text-right text-destructive"
              }
            >
              {l.side === "add" ? `+${l.absoluteLine}` : `-${l.absoluteLine}`}
            </span>
            <span className="min-w-0 flex-1 truncate text-foreground">
              {l.raw}
            </span>
            <button
              type="button"
              aria-label={`Discard line ${l.absoluteLine}`}
              title={`Discard this ${l.side === "add" ? "added" : "deleted"} line`}
              onClick={() =>
                onDiscardLine({
                  hunk,
                  side: l.side,
                  absoluteLine: l.absoluteLine,
                  rawLine: l.raw,
                })
              }
              className="opacity-0 transition group-hover/row:opacity-100 hover:text-destructive"
            >
              <Trash2 size={11} />
            </button>
          </li>
        ))}
      </ul>
    </details>
  );
}

// --- helpers -----------------------------------------------------

/**
 * Rebuild the `diff --git` + `--- a/` + `+++ b/` preamble from a
 * file path. This is what `@pierre/diffs`'s `PatchDiff` expects to
 * see before any `@@` header — without it the hunk is parsed but
 * not associated with a file name and the syntax-highlighting
 * language inference fails.
 *
 * Trailing newline included so callers can concatenate `\${hdr}@@
 * …`.
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
 * Stitch a per-file sub-patch back together from parsed hunks. This
 * is only used as input to the pointer detector — the detector
 * scans the whole thing for the magic signatures, so we need a
 * single string that contains every hunk body. We can't just use
 * the original input because the caller may have passed a multi-
 * file patch; `parsePatch` already split it, so we rebuild per
 * file.
 */
function buildFileSubPatch(file: ParsedFile, fileHeader: string): string {
  if (file.hunks.length === 0) return fileHeader;
  const out: string[] = [fileHeader];
  for (const hunk of file.hunks) {
    out.push(hunkHeader(hunk));
    out.push("\n");
    out.push(hunk.body);
    if (!hunk.body.endsWith("\n")) out.push("\n");
  }
  return out.join("");
}

function hunkHeader(h: {
  oldStart: number;
  oldLines: number;
  newStart: number;
  newLines: number;
}): string {
  // Match git's convention: omit the count when it's exactly 1.
  const oldPart = h.oldLines === 1 ? String(h.oldStart) : `${h.oldStart},${h.oldLines}`;
  const newPart = h.newLines === 1 ? String(h.newStart) : `${h.newStart},${h.newLines}`;
  return `@@ -${oldPart} +${newPart} @@`;
}
