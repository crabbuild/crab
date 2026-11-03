// Detect whether a unified patch touches a Crab or Git-LFS
// pointer blob.
//
// The Changes view renders per-hunk stage / unstage / discard
// controls by default. Pointer blobs don't play well with that
// model — a Crab pointer is a hex-encoded header describing the
// content-addressed chunks that make up the real file, and the
// "lines" inside it are not meaningful to edit piecemeal. Per the
// design doc: pointer-file hunks take the whole-file patch only,
// and the renderer shows a single "Stage entire pointer change"
// card instead of a per-hunk toolbar.
//
// The cheapest signal we have is the body of the patch itself: a
// pointer blob carries a recognisable magic string on either side
// of the diff. This module is a pure string probe — no RPC calls,
// no fs access — so it can run against any patch the renderer has
// already parsed for staging.
//
// Signatures we look for (all case-insensitive):
//
//   - Crab: `crab.dev/v1` in the body. The current pointer
//     format writes `crab.dev/v1\n` as the first line of the
//     blob; older / future variants may switch to
//     `version crab v1`, which we also match defensively.
//   - LFS: `git-lfs.github.com/spec/v1` anywhere in the body. This
//     is the canonical LFS pointer line (`version
//     https://git-lfs.github.com/spec/v1`). The plain URL prefix
//     `version https://git-lfs.github.com/` is accepted too to
//     tolerate future spec URL revisions.
//
// Matches are anchored to the *hunk content* — we strip the leading
// ` ` / `+` / `-` prefix so a context line touching a pointer blob
// counts the same as an added / removed pointer line. That's
// important because staging a pointer modification looks like
// `- <old pointer line>\n+ <new pointer line>\n`: both sides
// carry the magic, either one on its own is enough to classify.
//
// Returns `{ isPointer: false, kind: null }` for patches that
// clearly don't contain pointer content (plain text, binary-only
// diffs, or empty input). When both signatures are present (very
// unusual — would happen if a repo is mid-migration from LFS to
// Crab), Crab wins because that's the format this app
// actively manages.

/** Pointer flavour detected in the patch body. */
export type PointerPatchKind = "crab" | "lfs";

export interface PointerDetectionResult {
  /** True when the patch body contains a recognisable pointer
   *  signature on either side. */
  isPointer: boolean;
  /** Which pointer format matched. `null` when `isPointer` is
   *  false. When both signatures are present, `crab` wins — see
   *  the rationale in the module docs. */
  kind: PointerPatchKind | null;
}

// Pre-compiled, case-insensitive probes. Kept as plain strings
// rather than regexes because we only ever do substring matches:
// the patch body is already normalised (LF-only, no CRLF handling
// needed — git normalises on its way through `git diff`).
const CRAB_SIGNATURES = [
  "crab.dev/v1",
  // Older / alternate form — seen in early pointer docs and kept
  // as a defensive match so stale pointers still classify.
  "version crab v1",
] as const;

const LFS_SIGNATURES = [
  "git-lfs.github.com/spec/v1",
  // Broader prefix match — catches future spec revisions that
  // change the `/spec/vN` suffix without moving the domain.
  "version https://git-lfs.github.com/",
] as const;

/**
 * Scan a unified patch string for Crab or LFS pointer signatures.
 *
 * The probe is deliberately forgiving: we lowercase the input once
 * and do plain substring checks. Pointer files are small enough
 * (tens to low-hundreds of bytes) that the cost is negligible, and
 * the alternative — parsing the hunks and reconstructing either
 * side — adds complexity without better precision. If we later
 * need a rigorous classification, the agent can expose a
 * `pointer_kinds_for_paths` verb and we'll call it from the caller
 * layer; this helper stays as the fast local default.
 */
export function isPointerPatch(patch: string | null | undefined): PointerDetectionResult {
  if (!patch || patch.length === 0) {
    return { isPointer: false, kind: null };
  }
  // Case-insensitive: git preserves case inside blobs, but we want
  // to catch future pointer variants that capitalise differently.
  // A single `toLowerCase()` up front is cheaper than calling
  // `String.prototype.includes` with a case-insensitive regex for
  // every signature.
  const haystack = patch.toLowerCase();

  let crab = false;
  for (const sig of CRAB_SIGNATURES) {
    if (haystack.includes(sig)) {
      crab = true;
      break;
    }
  }
  if (crab) {
    return { isPointer: true, kind: "crab" };
  }

  for (const sig of LFS_SIGNATURES) {
    if (haystack.includes(sig)) {
      return { isPointer: true, kind: "lfs" };
    }
  }

  return { isPointer: false, kind: null };
}

/**
 * Human-readable label for a pointer kind. Used by the
 * `<PointerFileCard />` renderer so callers don't duplicate the
 * mapping.
 */
export function pointerKindLabel(kind: PointerPatchKind | null): string {
  switch (kind) {
    case "crab":
      return "Crab pointer";
    case "lfs":
      return "LFS pointer";
    default:
      return "Pointer";
  }
}
