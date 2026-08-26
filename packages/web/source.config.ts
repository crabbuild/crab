import { defineDocs, defineConfig } from "fumadocs-mdx/config"
import { pageSchema } from "fumadocs-core/source/schema"
import { remarkMermaid } from "./lib/remark-mermaid.mjs"
import { z } from "zod"

/**
 * CLI docs use `# Title` as the first
 * heading instead of YAML frontmatter. Make `title` optional so Fumadocs
 * can derive it from the heading via remarkHeading.
 *
 * All content files use .mdx extension for full MDX component support.
 * Angle brackets and curly braces in prose are escaped as HTML entities.
 */
const externalDocsSchema = pageSchema.extend({
  title: z.string().optional(),
  meta: z
    .object({
      contentType: z.enum([
        "Tutorial",
        "How-to",
        "Reference",
        "Conceptual",
        "Troubleshooting",
        "Landing",
      ]),
      goal: z.string(),
      audience: z.string(),
    })
    .optional(),
})

export const cliDocs = defineDocs({
  dir: "content/docs/cli",
  docs: {
    schema: externalDocsSchema,
  },
})

const guideSchema = pageSchema.extend({
  date: z.string().optional(),
  author: z.string().optional(),
  category: z
    .enum(["product", "tutorial", "architecture", "use-case", "release"])
    .optional(),
  tags: z.array(z.string()).optional(),
  excerpt: z.string().optional(),
  level: z.enum(["beginner", "intermediate", "deep-dive"]).optional(),
  path: z
    .enum([
      "start-here",
      "first-workflow",
      "core-internals",
      "advanced-operations",
      "migration",
    ])
    .optional(),
  order: z.number().optional(),
  concepts: z.array(z.string()).optional(),
  prerequisites: z.array(z.string()).optional(),
  outcome: z.string().optional(),
  diagramType: z.string().optional(),
})

export const blog = defineDocs({
  dir: "content/blog",
  docs: {
    schema: guideSchema.extend({
      date: z.string(),
      author: z.string(),
      category: z.enum([
        "product",
        "tutorial",
        "architecture",
        "use-case",
        "release",
      ]),
      tags: z.array(z.string()).min(1),
      excerpt: z.string(),
      level: z.enum(["beginner", "intermediate", "deep-dive"]),
      audience: z.string(),
      presentation: z.literal("feature").optional(),
    }),
  },
})

export const library = defineDocs({
  dir: "content/library",
  docs: {
    schema: guideSchema.extend({
      knowledgeCheck: z.object({
        question: z.string(),
        options: z.array(z.string()).min(2),
        answer: z.number().int().nonnegative(),
        explanation: z.string(),
      }),
    }),
  },
})

export default defineConfig({
  mdxOptions: {
    remarkPlugins: [remarkMermaid],
    rehypeCodeOptions: {
      themes: {
        light: "github-light",
        dark: "github-dark",
      },
      fallbackLanguage: "text",
    },
  },
})
