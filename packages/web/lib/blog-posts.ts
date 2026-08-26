import { blogSource } from "@/lib/blog-source"
import {
  calculateReadingTime,
  countWordsFromMdxFile,
} from "@/lib/mdx-reading-time"

export type BlogLevel = "Beginner" | "Intermediate" | "Deep Dive"

export interface BlogPostMeta {
  slug: string
  title: string
  description: string
  excerpt: string
  date: string
  author: string
  category: "Product" | "Tutorial" | "Architecture" | "Use Case" | "Release"
  tags: string[]
  level: BlogLevel
  audience: string
  readingTimeMinutes: number
}

type RawBlogData = {
  title?: string
  description?: string
  date: string
  author: string
  category: "product" | "tutorial" | "architecture" | "use-case" | "release"
  tags: string[]
  excerpt: string
  level: "beginner" | "intermediate" | "deep-dive"
  audience: string
}

const categoryLabels: Record<
  RawBlogData["category"],
  BlogPostMeta["category"]
> = {
  product: "Product",
  tutorial: "Tutorial",
  architecture: "Architecture",
  "use-case": "Use Case",
  release: "Release",
}

const levelLabels: Record<RawBlogData["level"], BlogLevel> = {
  beginner: "Beginner",
  intermediate: "Intermediate",
  "deep-dive": "Deep Dive",
}

export function getBlogPosts(): BlogPostMeta[] {
  return blogSource
    .getPages()
    .map((page) => {
      const data = page.data as RawBlogData
      const slug = page.slugs.join("/")
      const wordCount = countWordsFromMdxFile("blog", slug)

      return {
        slug,
        title: data.title ?? "Untitled",
        description: data.description ?? data.excerpt,
        excerpt: data.excerpt,
        date: data.date,
        author: data.author,
        category: categoryLabels[data.category],
        tags: data.tags,
        level: levelLabels[data.level],
        audience: data.audience,
        readingTimeMinutes: calculateReadingTime(wordCount),
      } satisfies BlogPostMeta
    })
    .sort((a, b) => new Date(b.date).getTime() - new Date(a.date).getTime())
}

export function getBlogPost(slug: string): BlogPostMeta | undefined {
  return getBlogPosts().find((post) => post.slug === slug)
}
