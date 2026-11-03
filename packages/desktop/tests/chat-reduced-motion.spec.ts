/**
 * Reduced motion tests — verify that every chat animation class has a
 * @media (prefers-reduced-motion: reduce) guard in index.css.
 *
 * Validates: Requirements NFR2
 */

import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const cssPath = resolve(__dirname, "../src/index.css");
const css = readFileSync(cssPath, "utf-8");

/**
 * Extract the content of all `@media (prefers-reduced-motion: reduce)` blocks
 * from the CSS source. Handles nested braces within the media query block.
 */
function extractReducedMotionBlocks(source: string): string {
  const blocks: string[] = [];
  const regex = /@media\s*\(\s*prefers-reduced-motion\s*:\s*reduce\s*\)/g;
  let match: RegExpExecArray | null;

  while ((match = regex.exec(source)) !== null) {
    const startIndex = source.indexOf("{", match.index + match[0].length);
    if (startIndex === -1) continue;

    let depth = 1;
    let i = startIndex + 1;
    while (i < source.length && depth > 0) {
      if (source[i] === "{") depth++;
      if (source[i] === "}") depth--;
      i++;
    }
    blocks.push(source.slice(startIndex + 1, i - 1));
  }

  return blocks.join("\n");
}

const reducedMotionContent = extractReducedMotionBlocks(css);

describe("prefers-reduced-motion guards in index.css", () => {
  it(".chat-collapsible has a reduced motion guard disabling transitions", () => {
    expect(reducedMotionContent).toContain(".chat-collapsible");
    // Should disable the transition
    const chatCollapsibleBlock = reducedMotionContent.slice(
      reducedMotionContent.indexOf(".chat-collapsible"),
    );
    expect(chatCollapsibleBlock).toMatch(/transition\s*:\s*none/);
  });

  it(".streaming-cursor has a reduced motion guard disabling animation", () => {
    expect(reducedMotionContent).toContain(".streaming-cursor");
    const cursorBlock = reducedMotionContent.slice(
      reducedMotionContent.indexOf(".streaming-cursor"),
    );
    expect(cursorBlock).toMatch(/animation\s*:\s*none/);
  });

  it(".turn-header-collapse has a reduced motion guard disabling transitions", () => {
    expect(reducedMotionContent).toContain(".turn-header-collapse");
    const headerBlock = reducedMotionContent.slice(
      reducedMotionContent.indexOf(".turn-header-collapse"),
    );
    expect(headerBlock).toMatch(/transition\s*:\s*none/);
  });

  it(".tool-group-slide-in has a reduced motion guard disabling animation", () => {
    expect(reducedMotionContent).toContain(".tool-group-slide-in");
    const slideInBlock = reducedMotionContent.slice(
      reducedMotionContent.indexOf(".tool-group-slide-in"),
    );
    expect(slideInBlock).toMatch(/animation\s*:\s*none/);
  });
});
