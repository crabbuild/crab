import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { DiscussionMarkdown } from "@/components/discussion/markdown";

describe("discussion Markdown", () => {
  it("renders GFM formatting while treating raw HTML and dangerous URLs as untrusted", () => {
    const html = renderToStaticMarkup(
      <DiscussionMarkdown>
        {
          "## Review\n\n- [x] tested\n\n| File | Result |\n| --- | --- |\n| src | passed |\n\n<script>alert(1)</script>\n\n[unsafe](javascript:alert(1))\n\n<img src=x onerror=alert(1)>"
        }
      </DiscussionMarkdown>,
    );
    expect(html).toContain("<h2>Review</h2>");
    expect(html).toContain("<table>");
    expect(html).toContain('type="checkbox"');
    expect(html).not.toMatch(/<script|onerror=|javascript:/i);
  });

  it("presents external images as explicit links without loading third-party image requests", () => {
    const html = renderToStaticMarkup(
      <DiscussionMarkdown>
        {"![Design proposal](https://example.com/design.png)"}
      </DiscussionMarkdown>,
    );
    expect(html).toContain('href="https://example.com/design.png"');
    expect(html).toContain("Design proposal");
    expect(html).not.toContain("<img");
  });

  it("renders Markdown alerts and collapsible sections through the safe renderer", () => {
    const html = renderToStaticMarkup(
      <DiscussionMarkdown>
        {
          "<details>\n<summary>Test results</summary>\n\n> [!TIP]\n> **Run** the suite.\n\n<img src=x onerror=alert(1)>\n\n</details>"
        }
      </DiscussionMarkdown>,
    );
    expect(html).toMatch(
      /<details><summary>Test results<\/summary>[\s\S]*data-kind="tip"[\s\S]*<strong>Run<\/strong>[\s\S]*<\/details>/,
    );
    expect(html).not.toMatch(/<img|onerror=/i);
  });
});
