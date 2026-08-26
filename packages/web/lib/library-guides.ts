import fs from "node:fs"
import path from "node:path"

import { librarySource } from "@/lib/library-source"
import {
  LIBRARY_PATHS,
  calculateReadingTime,
  type LibraryPathKey,
  type LibraryGuideLevel,
  type LibraryGuideMeta,
} from "@/lib/library"

type RawLibraryData = {
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
  knowledgeCheck: {
    question: string
    options: string[]
    answer: number
    explanation: string
  }
}

const CATEGORY_MAP: Record<string, LibraryGuideMeta["category"]> = {
  product: "Product",
  tutorial: "Tutorial",
  architecture: "Architecture",
  "use-case": "Use Case",
  release: "Release",
}

const LEVEL_MAP: Record<string, LibraryGuideLevel> = {
  beginner: "Beginner",
  intermediate: "Intermediate",
  "deep-dive": "Deep Dive",
}

const PATH_KEYS = new Set<string>(LIBRARY_PATHS.map((entry) => entry.key))

export function mapLibraryCategory(
  raw: string | undefined
): LibraryGuideMeta["category"] {
  return CATEGORY_MAP[raw ?? ""] ?? "Product"
}

function mapLevel(raw: string | undefined): LibraryGuideLevel {
  return LEVEL_MAP[raw ?? ""] ?? "Beginner"
}

function mapPath(raw: string | undefined): LibraryPathKey {
  return PATH_KEYS.has(raw ?? "") ? (raw as LibraryPathKey) : "start-here"
}

/**
 * Counts words in an MDX file's prose content, excluding frontmatter,
 * fenced code blocks, and Mermaid diagrams.
 */
export function countWordsFromGuideFile(slug: string): number {
  const filePath = path.join(process.cwd(), "content", "library", `${slug}.mdx`)

  try {
    const content = fs.readFileSync(filePath, "utf-8")
    const withoutFrontmatter = content.replace(/^---[\s\S]*?---/, "")
    const withoutCode = withoutFrontmatter.replace(/```[\s\S]*?```/g, "")

    return withoutCode.split(/\s+/).filter((word) => word.length > 0).length
  } catch {
    return 200
  }
}

export function getLibraryGuides(): LibraryGuideMeta[] {
  return librarySource
    .getPages()
    .map((page) => {
      const data = page.data as RawLibraryData
      const slug = page.slugs[0] ?? ""
      const wordCount = countWordsFromGuideFile(slug)
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
        category: mapLibraryCategory(data.category),
        tags: data.tags ?? [],
        readingTimeMinutes: calculateReadingTime(wordCount),
        level: mapLevel(data.level),
        path: mapPath(data.path),
        pathOrder: data.order ?? 999,
        concepts: data.concepts ?? [],
        prerequisites: data.prerequisites ?? [],
        outcome: data.outcome ?? description,
        diagramType: data.diagramType,
        knowledgeCheck: data.knowledgeCheck,
      } satisfies LibraryGuideMeta
    })
    .sort((a, b) => {
      const pathDelta =
        LIBRARY_PATHS.find((entry) => entry.key === a.path)!.order -
        LIBRARY_PATHS.find((entry) => entry.key === b.path)!.order

      if (pathDelta !== 0) return pathDelta
      if (a.pathOrder !== b.pathOrder) return a.pathOrder - b.pathOrder

      return new Date(b.date).getTime() - new Date(a.date).getTime()
    })
}

export function getLibraryGuide(slug: string): LibraryGuideMeta | undefined {
  return getLibraryGuides().find((post) => post.slug === slug)
}
