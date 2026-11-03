/**
 * Unit tests for the AnsiText helper.
 *
 * Verifies that ANSI / CSI escape sequences in a string are parsed
 * into a React tree where each coloured / decorated fragment becomes
 * a `<span>` carrying the matching `ansi-*` class names. The actual
 * pixel rendering lives in src/index.css and is not tested here.
 */

import { describe, it, expect } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import * as React from "react";

import { AnsiText, ansiToReact, stripAnsiToText } from "../src/lib/ansi-text";

const ESC = "\x1b";

/** Renders a React element to plain HTML string for assertion. */
function render(node: React.ReactElement): string {
  return renderToStaticMarkup(node);
}

describe("ansiToReact", () => {
  it("returns no nodes for empty input", () => {
    expect(ansiToReact("")).toEqual([]);
  });

  it("passes plain text through as a single fragment with no spans", () => {
    const html = render(React.createElement(AnsiText, null, "hello world"));
    expect(html).toBe("hello world");
  });

  it("renders a red-coloured fragment as a span with ansi-red-fg", () => {
    const text = `${ESC}[31mfailure${ESC}[0m and recovery`;
    const html = render(React.createElement(AnsiText, null, text));
    expect(html).toContain('class="ansi-red-fg"');
    expect(html).toContain(">failure</span>");
    expect(html).toContain(" and recovery");
  });

  it("maps bright variants to ansi-bright-*-fg", () => {
    const text = `${ESC}[91mbright red${ESC}[0m`;
    const html = render(React.createElement(AnsiText, null, text));
    expect(html).toContain('class="ansi-bright-red-fg"');
  });

  it("renders bold + colour as space-separated classes", () => {
    const text = `${ESC}[1;32mbold green${ESC}[0m`;
    const html = render(React.createElement(AnsiText, null, text));
    // Class order is fg first, then decorations, per the helper.
    expect(html).toMatch(/class="ansi-green-fg ansi-bold"/);
    expect(html).toContain(">bold green</span>");
  });

  it("renders italic and underline as standalone classes", () => {
    const italicText = `${ESC}[3mitalic${ESC}[0m`;
    expect(render(React.createElement(AnsiText, null, italicText))).toContain(
      'class="ansi-italic"',
    );

    const underlineText = `${ESC}[4munderline${ESC}[0m`;
    expect(render(React.createElement(AnsiText, null, underlineText))).toContain(
      'class="ansi-underline"',
    );
  });

  it("renders a multi-segment string as multiple spans", () => {
    const text = `${ESC}[31mred${ESC}[0m ${ESC}[32mgreen${ESC}[0m`;
    const html = render(React.createElement(AnsiText, null, text));
    expect(html).toContain('class="ansi-red-fg"');
    expect(html).toContain('class="ansi-green-fg"');
    // The space between them should not be wrapped in a span.
    expect(html).toMatch(/<\/span> <span/);
  });

  it("escapes HTML special characters in content", () => {
    const text = `${ESC}[31m<script>alert(1)</script>${ESC}[0m`;
    const html = render(React.createElement(AnsiText, null, text));
    // React's escaping is what we rely on here — no raw <script> should
    // ever appear in the output.
    expect(html).not.toContain("<script>");
    expect(html).toContain("&lt;script&gt;");
  });

  it("attaches the className prop to the wrapper span when provided", () => {
    const html = render(
      React.createElement(AnsiText, { className: "wrap-me" }, "plain"),
    );
    expect(html).toBe('<span class="wrap-me">plain</span>');
  });
});

describe("stripAnsiToText", () => {
  it("returns empty string for empty input", () => {
    expect(stripAnsiToText("")).toBe("");
  });

  it("strips colour escapes and yields plain text", () => {
    const text = `${ESC}[31merror${ESC}[0m: ${ESC}[1msomething broke${ESC}[0m`;
    expect(stripAnsiToText(text)).toBe("error: something broke");
  });

  it("leaves text without escapes untouched", () => {
    expect(stripAnsiToText("plain string")).toBe("plain string");
  });
});
