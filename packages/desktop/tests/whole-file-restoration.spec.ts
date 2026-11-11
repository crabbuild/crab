import { describe, expect, it, vi } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { parsePatch } from "../src/lib/parse-patch";
import { StagingZones } from "../src/components/StagingZones";
import { BatchActionBar } from "../src/components/BatchActionBar";
import { InlinePatchDiff } from "../src/components/InlinePatchDiff";

vi.mock("../src/hooks/useEffectiveTheme", () => ({ useEffectiveTheme: () => "dark" }));
vi.mock("../src/components/PatchDiff", () => ({ PatchDiff: () => null }));
const binary = "diff --git a/model[1].bin b/model[1].bin\nindex 123..456 100644\nBinary files a/model[1].bin and b/model[1].bin differ\n";

describe("whole-file controls without text hunks", () => {
  it.each([binary, "diff --git a/run b/run\nold mode 100644\nnew mode 100755\n"])("retains an addressable file for binary and mode changes", patch => {
    const parsed = parsePatch(patch);
    expect(parsed.files.map(file => [file.path, file.hunks.length])).toEqual([[patch === binary ? "model[1].bin" : "run", 0]]);
    expect(parsed.hunks).toEqual([]);
  });
  it("disables row and batch discard when native restoration is unavailable", () => {
    const zones = renderToStaticMarkup(createElement(StagingZones, {
      entries: [{ path: "model.bin", code: " M" }], fileStates: { "model.bin": "empty" },
      selectedPath: null, multiSelected: new Set<string>(), togglingPaths: new Set<string>(),
      onSelect: vi.fn(), onDoubleClick: vi.fn(), onToggleFile: vi.fn(), onContextMenu: vi.fn(),
    }));
    const batch = renderToStaticMarkup(createElement(BatchActionBar, { count: 2, onStage: vi.fn(), onUnstage: vi.fn(), onClear: vi.fn() }));
    expect(zones).toMatch(/<button(?=[^>]*aria-label="Discard changes in model.bin")(?=[^>]*disabled)[^>]*>/);
    expect(batch).toMatch(/<button(?=[^>]*title="Discard selected files \(Delete\)")(?=[^>]*disabled)[^>]*>/);
  });
  it("offers an enabled discard only when a verified whole-file owner is supplied", () => {
    const props = { patch: binary, onStageHunk: vi.fn(), onDiscardHunk: vi.fn(), onStageFile: vi.fn() };
    const unavailable = renderToStaticMarkup(createElement(InlinePatchDiff, props));
    const available = renderToStaticMarkup(createElement(InlinePatchDiff, { ...props, onDiscardFile: vi.fn() }));
    expect(unavailable).toMatch(/<button(?=[^>]*aria-label="Discard entire file change")(?=[^>]*disabled)[^>]*>/);
    expect(available).not.toMatch(/<button(?=[^>]*aria-label="Discard entire file change")(?=[^>]*disabled)[^>]*>/);
    expect(available).toContain("Binary or mode change");
  });
});
