export interface LibraryGuideMeta {
  slug: string
  title: string
  description: string
  date: string
  author: {
    name: string
    bio?: string
    avatar?: string
  }
  category: "Product" | "Tutorial" | "Architecture" | "Use Case" | "Release"
  tags: string[]
  readingTimeMinutes: number
  level: LibraryGuideLevel
  path: LibraryPathKey
  pathOrder: number
  concepts: string[]
  prerequisites: string[]
  outcome: string
  diagramType?: string
  knowledgeCheck: KnowledgeCheckData
}

export interface KnowledgeCheckData {
  question: string
  options: string[]
  answer: number
  explanation: string
}

export type LibraryGuideLevel = "Beginner" | "Intermediate" | "Deep Dive"

export type LibraryPathKey =
  | "start-here"
  | "first-workflow"
  | "core-internals"
  | "advanced-operations"

export interface LibraryPath {
  key: LibraryPathKey
  label: string
  shortLabel: string
  description: string
  audience: string
  order: number
}

export const LIBRARY_PATHS: LibraryPath[] = [
  {
    key: "start-here",
    label: "Start Here",
    shortLabel: "Start",
    description:
      "Build the basic Crab mental model before choosing a workflow.",
    audience: "New evaluators",
    order: 1,
  },
  {
    key: "first-workflow",
    label: "First Workflow",
    shortLabel: "Workflow",
    description:
      "Install Crab, push large files, and understand how it fits Git.",
    audience: "Hands-on users",
    order: 2,
  },
  {
    key: "core-internals",
    label: "Core Internals",
    shortLabel: "Internals",
    description:
      "Follow the storage, deduplication, cache, and hydration pipeline.",
    audience: "Technical reviewers",
    order: 3,
  },
  {
    key: "advanced-operations",
    label: "Advanced Operations",
    shortLabel: "Operate",
    description:
      "Reason about consistency, cleanup, cost, large repos, and Git LFS migration.",
    audience: "Platform and migration teams",
    order: 4,
  },
]

export const LIBRARY_LEVELS: LibraryGuideLevel[] = [
  "Beginner",
  "Intermediate",
  "Deep Dive",
]

export function getLibraryPath(key: LibraryPathKey): LibraryPath {
  return LIBRARY_PATHS.find((path) => path.key === key) ?? LIBRARY_PATHS[0]
}

export function getPathGuides(
  path: LibraryPathKey,
  posts: LibraryGuideMeta[]
): LibraryGuideMeta[] {
  return posts
    .filter((post) => post.path === path)
    .sort((a, b) => a.pathOrder - b.pathOrder)
}

export function getAdjacentPathGuides(
  post: LibraryGuideMeta,
  posts: LibraryGuideMeta[]
): {
  previous?: LibraryGuideMeta
  next?: LibraryGuideMeta
} {
  const pathPosts = getPathGuides(post.path, posts)
  const index = pathPosts.findIndex((candidate) => candidate.slug === post.slug)

  if (index < 0) return {}

  return {
    previous: pathPosts[index - 1],
    next: pathPosts[index + 1],
  }
}

/**
 * Calculates reading time in minutes from a word count.
 * Uses a rate of 200 words per minute, rounded up, with a minimum of 1.
 */
export function calculateReadingTime(wordCount: number): number {
  if (wordCount <= 0) return 1
  return Math.max(1, Math.ceil(wordCount / 200))
}

/**
 * Filters posts by category. Returns all posts when category is "All".
 */
export function filterByCategory(
  category: string,
  posts: LibraryGuideMeta[]
): LibraryGuideMeta[] {
  if (category === "All") return posts
  return posts.filter((post) => post.category === category)
}

/**
 * Filters posts by tag. Returns all posts when tag is empty or undefined.
 */
export function filterByTag(
  tag: string | undefined,
  posts: LibraryGuideMeta[]
): LibraryGuideMeta[] {
  if (!tag) return posts
  const normalized = tag.toLowerCase()
  return posts.filter(
    (post) =>
      post.tags.some((t) => t.toLowerCase() === normalized) ||
      post.concepts.some((concept) => concept.toLowerCase() === normalized)
  )
}

/**
 * Searches posts by case-insensitive substring match in title, description, or tags.
 * Returns all posts if query is empty after trimming.
 */
export function searchPosts(
  query: string,
  posts: LibraryGuideMeta[]
): LibraryGuideMeta[] {
  const trimmed = query.trim().toLowerCase()
  if (trimmed === "") return posts
  return posts.filter(
    (post) =>
      post.title.toLowerCase().includes(trimmed) ||
      post.description.toLowerCase().includes(trimmed) ||
      post.outcome.toLowerCase().includes(trimmed) ||
      post.level.toLowerCase().includes(trimmed) ||
      getLibraryPath(post.path).label.toLowerCase().includes(trimmed) ||
      post.concepts.some((concept) =>
        concept.toLowerCase().includes(trimmed)
      ) ||
      post.tags.some((t) => t.toLowerCase().includes(trimmed))
  )
}

export function filterByPath(
  path: string,
  posts: LibraryGuideMeta[]
): LibraryGuideMeta[] {
  if (path === "All") return posts
  return posts.filter((post) => post.path === path)
}

export function filterByLevel(
  level: string,
  posts: LibraryGuideMeta[]
): LibraryGuideMeta[] {
  if (level === "All") return posts
  return posts.filter((post) => post.level === level)
}

/**
 * Returns up to 3 related posts from the same category, excluding the
 * given post. Prefer the same learning path and shared concepts, then
 * fall back to category and recency.
 */
export function getRelatedGuides(
  post: LibraryGuideMeta,
  allPosts: LibraryGuideMeta[]
): LibraryGuideMeta[] {
  return allPosts
    .filter((p) => p.slug !== post.slug)
    .map((candidate) => {
      const sharedConcepts = candidate.concepts.filter((concept) =>
        post.concepts.includes(concept)
      ).length
      const pathScore = candidate.path === post.path ? 4 : 0
      const categoryScore = candidate.category === post.category ? 2 : 0

      return {
        candidate,
        score: pathScore + categoryScore + sharedConcepts,
      }
    })
    .sort((a, b) => {
      if (b.score !== a.score) return b.score - a.score
      return (
        new Date(b.candidate.date).getTime() -
        new Date(a.candidate.date).getTime()
      )
    })
    .map(({ candidate }) => candidate)
    .slice(0, 3)
}
