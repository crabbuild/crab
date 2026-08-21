export interface BlogPostMeta {
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
  level: BlogPostLevel
  path: BlogLearningPathKey
  pathOrder: number
  concepts: string[]
  prerequisites: string[]
  outcome: string
  diagramType?: string
}

export type BlogPostLevel = "Beginner" | "Intermediate" | "Deep Dive"

export type BlogLearningPathKey =
  | "start-here"
  | "first-workflow"
  | "core-internals"
  | "advanced-operations"

export interface BlogLearningPath {
  key: BlogLearningPathKey
  label: string
  shortLabel: string
  description: string
  audience: string
  order: number
}

export const BLOG_LEARNING_PATHS: BlogLearningPath[] = [
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

export const BLOG_LEVELS: BlogPostLevel[] = [
  "Beginner",
  "Intermediate",
  "Deep Dive",
]

export function getLearningPath(key: BlogLearningPathKey): BlogLearningPath {
  return (
    BLOG_LEARNING_PATHS.find((path) => path.key === key) ??
    BLOG_LEARNING_PATHS[0]
  )
}

export function getPathPosts(
  path: BlogLearningPathKey,
  posts: BlogPostMeta[]
): BlogPostMeta[] {
  return posts
    .filter((post) => post.path === path)
    .sort((a, b) => a.pathOrder - b.pathOrder)
}

export function getAdjacentPathPosts(
  post: BlogPostMeta,
  posts: BlogPostMeta[]
): {
  previous?: BlogPostMeta
  next?: BlogPostMeta
} {
  const pathPosts = getPathPosts(post.path, posts)
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
  posts: BlogPostMeta[]
): BlogPostMeta[] {
  if (category === "All") return posts
  return posts.filter((post) => post.category === category)
}

/**
 * Filters posts by tag. Returns all posts when tag is empty or undefined.
 */
export function filterByTag(
  tag: string | undefined,
  posts: BlogPostMeta[]
): BlogPostMeta[] {
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
  posts: BlogPostMeta[]
): BlogPostMeta[] {
  const trimmed = query.trim().toLowerCase()
  if (trimmed === "") return posts
  return posts.filter(
    (post) =>
      post.title.toLowerCase().includes(trimmed) ||
      post.description.toLowerCase().includes(trimmed) ||
      post.outcome.toLowerCase().includes(trimmed) ||
      post.level.toLowerCase().includes(trimmed) ||
      getLearningPath(post.path).label.toLowerCase().includes(trimmed) ||
      post.concepts.some((concept) =>
        concept.toLowerCase().includes(trimmed)
      ) ||
      post.tags.some((t) => t.toLowerCase().includes(trimmed))
  )
}

export function filterByPath(
  path: string,
  posts: BlogPostMeta[]
): BlogPostMeta[] {
  if (path === "All") return posts
  return posts.filter((post) => post.path === path)
}

export function filterByLevel(
  level: string,
  posts: BlogPostMeta[]
): BlogPostMeta[] {
  if (level === "All") return posts
  return posts.filter((post) => post.level === level)
}

/**
 * Returns up to 3 related posts from the same category, excluding the
 * given post. Prefer the same learning path and shared concepts, then
 * fall back to category and recency.
 */
export function getRelatedPosts(
  post: BlogPostMeta,
  allPosts: BlogPostMeta[]
): BlogPostMeta[] {
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
