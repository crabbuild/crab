import fs from "node:fs"
import path from "node:path"

import { blogSource } from "@/lib/blog-source"
import {
  BLOG_LEARNING_PATHS,
  calculateReadingTime,
  type BlogLearningPathKey,
  type BlogPostLevel,
  type BlogPostMeta,
} from "@/lib/blog"

type RawBlogData = {
  title?: string
  description?: string
  date?: string
  author?: string
  category?: string
  excerpt?: string
  tags?: string[]
  level?: string
  path?: string
  order?: number
  concepts?: string[]
  prerequisites?: string[]
  outcome?: string
  diagramType?: string
}

const CATEGORY_MAP: Record<string, BlogPostMeta["category"]> = {
  product: "Product",
  tutorial: "Tutorial",
  architecture: "Architecture",
  "use-case": "Use Case",
  release: "Release",
}

const LEVEL_MAP: Record<string, BlogPostLevel> = {
  beginner: "Beginner",
  intermediate: "Intermediate",
  "deep-dive": "Deep Dive",
}

const PATH_KEYS = new Set<string>(BLOG_LEARNING_PATHS.map((entry) => entry.key))

export function mapBlogCategory(raw: string | undefined): BlogPostMeta["category"] {
  return CATEGORY_MAP[raw ?? ""] ?? "Product"
}

function mapLevel(raw: string | undefined): BlogPostLevel {
  return LEVEL_MAP[raw ?? ""] ?? "Beginner"
}

function mapPath(raw: string | undefined): BlogLearningPathKey {
  return PATH_KEYS.has(raw ?? "")
    ? (raw as BlogLearningPathKey)
    : "start-here"
}

/**
 * Counts words in an MDX file's prose content, excluding frontmatter,
 * fenced code blocks, and Mermaid diagrams.
 */
export function countWordsFromFile(slug: string): number {
  const filePath = path.join(process.cwd(), "content", "blog", `${slug}.mdx`)

  try {
    const content = fs.readFileSync(filePath, "utf-8")
    const withoutFrontmatter = content.replace(/^---[\s\S]*?---/, "")
    const withoutCode = withoutFrontmatter.replace(/```[\s\S]*?```/g, "")

    return withoutCode.split(/\s+/).filter((word) => word.length > 0).length
  } catch {
    return 200
  }
}

export function getBlogPosts(): BlogPostMeta[] {
  return blogSource
    .getPages()
    .map((page) => {
      const data = page.data as RawBlogData
      const slug = page.slugs[0] ?? ""
      const wordCount = countWordsFromFile(slug)
      const description = data.excerpt ?? data.description ?? ""

      return {
        slug,
        title: data.title ?? "Untitled",
        description,
        date: data.date ?? "2025-01-01",
        author: {
          name: data.author ?? "Crab Team",
          bio: "Building serverless Git workflows for large files, cloud object storage, and fast technical teams.",
        },
        category: mapBlogCategory(data.category),
        tags: data.tags ?? [],
        readingTimeMinutes: calculateReadingTime(wordCount),
        level: mapLevel(data.level),
        path: mapPath(data.path),
        pathOrder: data.order ?? 999,
        concepts: data.concepts ?? [],
        prerequisites: data.prerequisites ?? [],
        outcome: data.outcome ?? description,
        diagramType: data.diagramType,
      } satisfies BlogPostMeta
    })
    .sort((a, b) => {
      const pathDelta =
        BLOG_LEARNING_PATHS.find((entry) => entry.key === a.path)!.order -
        BLOG_LEARNING_PATHS.find((entry) => entry.key === b.path)!.order

      if (pathDelta !== 0) return pathDelta
      if (a.pathOrder !== b.pathOrder) return a.pathOrder - b.pathOrder

      return new Date(b.date).getTime() - new Date(a.date).getTime()
    })
}

export function getBlogPost(slug: string): BlogPostMeta | undefined {
  return getBlogPosts().find((post) => post.slug === slug)
}
