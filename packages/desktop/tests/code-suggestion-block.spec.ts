import { describe, expect, it } from "vitest";
import { parseSuggestionFromBody } from "../src/components/review/CodeSuggestionBlock";

describe("parseSuggestionFromBody", () => {
  it("extracts suggestion from a standard suggestion block", () => {
    const body = `Here's a fix:\n\n\`\`\`suggestion\nconst x = useRef(0);\n\`\`\`\n\nLet me know what you think.`;
    const result = parseSuggestionFromBody(body);
    expect(result).not.toBeNull();
    expect(result!.suggestedCode).toBe("const x = useRef(0);");
  });

  it("returns null when no suggestion block is present", () => {
    const body = "This is a regular comment with no code suggestion.";
    expect(parseSuggestionFromBody(body)).toBeNull();
  });

  it("returns null for a regular code block without suggestion tag", () => {
    const body = "```typescript\nconst x = 1;\n```";
    expect(parseSuggestionFromBody(body)).toBeNull();
  });

  it("handles multi-line suggestions", () => {
    const body = "```suggestion\nconst a = 1;\nconst b = 2;\nconst c = a + b;\n```";
    const result = parseSuggestionFromBody(body);
    expect(result).not.toBeNull();
    expect(result!.suggestedCode).toBe("const a = 1;\nconst b = 2;\nconst c = a + b;");
  });

  it("handles empty suggestion (deletion)", () => {
    const body = "```suggestion\n```";
    const result = parseSuggestionFromBody(body);
    expect(result).not.toBeNull();
    expect(result!.suggestedCode).toBe("");
  });

  it("extracts only the first suggestion block when multiple exist", () => {
    const body = "```suggestion\nfirst\n```\n\n```suggestion\nsecond\n```";
    const result = parseSuggestionFromBody(body);
    expect(result).not.toBeNull();
    expect(result!.suggestedCode).toBe("first");
  });

  it("preserves indentation in suggested code", () => {
    const body = "```suggestion\n  if (x) {\n    return y;\n  }\n```";
    const result = parseSuggestionFromBody(body);
    expect(result).not.toBeNull();
    expect(result!.suggestedCode).toBe("  if (x) {\n    return y;\n  }");
  });
});
