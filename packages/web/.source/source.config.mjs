// source.config.ts
import { defineDocs, defineConfig } from "fumadocs-mdx/config";
import { pageSchema } from "fumadocs-core/source/schema";

// lib/remark-mermaid.mjs
import { visit } from "unist-util-visit";
function remarkMermaid() {
  return (tree) => {
    visit(tree, "code", (node, index, parent) => {
      if (node.lang !== "mermaid") return;
      if (index === void 0 || !parent) return;
      const value = node.value || "";
      parent.children[index] = {
        type: "mdxJsxFlowElement",
        name: "Mermaid",
        attributes: [
          {
            type: "mdxJsxAttribute",
            name: "chart",
            value
          }
        ],
        children: [],
        data: { _mdxExplicitJsx: true }
      };
    });
  };
}

// source.config.ts
import { z } from "zod";
var externalDocsSchema = pageSchema.extend({
  title: z.string().optional(),
  meta: z.object({
    contentType: z.enum([
      "Tutorial",
      "How-to",
      "Reference",
      "Conceptual",
      "Troubleshooting",
      "Landing"
    ]),
    goal: z.string(),
    audience: z.string()
  }).optional()
});
var cliDocs = defineDocs({
  dir: "content/docs/cli",
  docs: {
    schema: externalDocsSchema
  }
});
var guideSchema = pageSchema.extend({
  date: z.string().optional(),
  author: z.string().optional(),
  category: z.enum(["product", "tutorial", "architecture", "use-case", "release"]).optional(),
  tags: z.array(z.string()).optional(),
  excerpt: z.string().optional(),
  level: z.enum(["beginner", "intermediate", "deep-dive"]).optional(),
  path: z.enum([
    "start-here",
    "first-workflow",
    "core-internals",
    "advanced-operations",
    "migration"
  ]).optional(),
  order: z.number().optional(),
  concepts: z.array(z.string()).optional(),
  prerequisites: z.array(z.string()).optional(),
  outcome: z.string().optional(),
  diagramType: z.string().optional()
});
var blog = defineDocs({
  dir: "content/blog",
  docs: {
    schema: guideSchema
  }
});
var library = defineDocs({
  dir: "content/library",
  docs: {
    schema: guideSchema.extend({
      presentation: z.enum(["guide", "feature"]).optional(),
      knowledgeCheck: z.object({
        question: z.string(),
        options: z.array(z.string()).min(2),
        answer: z.number().int().nonnegative(),
        explanation: z.string()
      })
    })
  }
});
var source_config_default = defineConfig({
  mdxOptions: {
    remarkPlugins: [remarkMermaid],
    rehypeCodeOptions: {
      themes: {
        light: "github-light",
        dark: "github-dark"
      },
      fallbackLanguage: "text"
    }
  }
});
export {
  blog,
  cliDocs,
  source_config_default as default,
  library
};
