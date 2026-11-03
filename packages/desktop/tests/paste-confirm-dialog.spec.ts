import { describe, it, expect } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { PasteConfirmDialog } from "../src/components/PasteConfirmDialog";
import type { PasteAnalysis } from "../src/lib/terminal-paste-guard";

// Helper to render the dialog to static HTML for assertion.
function renderDialog(analysis: PasteAnalysis) {
  const html = renderToStaticMarkup(
    createElement(PasteConfirmDialog, {
      analysis,
      onConfirm: () => {},
      onCancel: () => {},
    }),
  );
  return html;
}

describe("PasteConfirmDialog", () => {
  describe("rendering", () => {
    it("renders a dialog with alertdialog role", () => {
      const analysis: PasteAnalysis = {
        requiresConfirmation: true,
        reasons: ["multi-line"],
        lineCount: 10,
        controlChars: [],
        dangerousPatterns: [],
        preview: "line1\nline2\nline3\n...\nline8\nline9\nline10",
      };
      const html = renderDialog(analysis);
      expect(html).toContain('role="alertdialog"');
      expect(html).toContain('aria-modal="true"');
    });

    it("shows line count for multi-line pastes", () => {
      const analysis: PasteAnalysis = {
        requiresConfirmation: true,
        reasons: ["multi-line"],
        lineCount: 42,
        controlChars: [],
        dangerousPatterns: [],
        preview: "first\n...\nlast",
      };
      const html = renderDialog(analysis);
      expect(html).toContain("42 lines");
    });

    it("shows control characters as badges", () => {
      const analysis: PasteAnalysis = {
        requiresConfirmation: true,
        reasons: ["control-chars"],
        lineCount: 1,
        controlChars: ["^C", "^["],
        dangerousPatterns: [],
        preview: "hello^Cworld^[end",
      };
      const html = renderDialog(analysis);
      expect(html).toContain("^C");
      expect(html).toContain("^[");
    });

    it("shows dangerous pattern warning", () => {
      const analysis: PasteAnalysis = {
        requiresConfirmation: true,
        reasons: ["dangerous-pattern"],
        lineCount: 1,
        controlChars: [],
        dangerousPatterns: ["rm\\s+-rf\\s+/"],
        preview: "rm -rf /",
      };
      const html = renderDialog(analysis);
      expect(html).toContain("Dangerous paste detected");
      expect(html).toContain("I understand the risk");
    });

    it("renders the preview content", () => {
      const analysis: PasteAnalysis = {
        requiresConfirmation: true,
        reasons: ["multi-line"],
        lineCount: 8,
        controlChars: [],
        dangerousPatterns: [],
        preview: "line1\nline2\nline3\n...\nline6\nline7\nline8",
      };
      const html = renderDialog(analysis);
      expect(html).toContain("line1");
      expect(html).toContain("line8");
      expect(html).toContain("...");
    });

    it("disables Paste button when dangerous patterns present and risk not acknowledged", () => {
      const analysis: PasteAnalysis = {
        requiresConfirmation: true,
        reasons: ["dangerous-pattern"],
        lineCount: 1,
        controlChars: [],
        dangerousPatterns: ["rm\\s+-rf\\s+/"],
        preview: "rm -rf /",
      };
      const html = renderDialog(analysis);
      // The Paste button should be disabled (has disabled attribute)
      expect(html).toContain("disabled");
    });

    it("does not show risk checkbox for non-dangerous pastes", () => {
      const analysis: PasteAnalysis = {
        requiresConfirmation: true,
        reasons: ["multi-line"],
        lineCount: 10,
        controlChars: [],
        dangerousPatterns: [],
        preview: "line1\n...\nline10",
      };
      const html = renderDialog(analysis);
      expect(html).not.toContain("I understand the risk");
    });

    it("shows Cancel and Paste buttons", () => {
      const analysis: PasteAnalysis = {
        requiresConfirmation: true,
        reasons: ["multi-line"],
        lineCount: 6,
        controlChars: [],
        dangerousPatterns: [],
        preview: "1\n2\n3\n4\n5\n6",
      };
      const html = renderDialog(analysis);
      expect(html).toContain("Cancel");
      expect(html).toContain("Paste");
    });

    it("highlights control characters in preview when control-chars reason present", () => {
      const analysis: PasteAnalysis = {
        requiresConfirmation: true,
        reasons: ["control-chars"],
        lineCount: 1,
        controlChars: ["^C"],
        dangerousPatterns: [],
        preview: "hello^Cworld",
      };
      const html = renderDialog(analysis);
      // Control chars should be wrapped in a span with warning styling
      expect(html).toContain("title=\"Control character: ^C\"");
    });

    it("shows description with all applicable reasons", () => {
      const analysis: PasteAnalysis = {
        requiresConfirmation: true,
        reasons: ["control-chars", "multi-line", "dangerous-pattern"],
        lineCount: 10,
        controlChars: ["^C"],
        dangerousPatterns: ["rm\\s+-rf\\s+/"],
        preview: "rm -rf /^C\n2\n3\n4\n5\n6\n7\n8\n9\n10",
      };
      const html = renderDialog(analysis);
      expect(html).toContain("dangerous command pattern");
      expect(html).toContain("control character");
      expect(html).toContain("Multi-line paste");
    });
  });
});
