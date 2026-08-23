import Link from "next/link"
import { Suspense } from "react"
import {
  ArrowRight,
  BookOpen,
  Briefcase,
  CheckCircle2,
  Clock,
  GitBranch,
  Layers,
  Network,
  Package,
  Rocket,
  Route,
  Search,
} from "lucide-react"

import { BlogIndexContent } from "@/components/blog/blog-index-content"
import { MarketingLayout } from "@/components/marketing-layout"
import { Reveal } from "@/components/marketing/reveal"
import { Badge } from "@/components/ui/badge"
import {
  BLOG_LEARNING_PATHS,
  getLearningPath,
  getPathPosts,
  type BlogLearningPath,
  type BlogLearningPathKey,
  type BlogPostMeta,
} from "@/lib/blog"
import { getBlogPosts } from "@/lib/blog-posts"
import { cn } from "@/lib/utils"
import { createPageMetadata } from "@/lib/metadata"

export const metadata = createPageMetadata({
  title: "Blog",
  description:
    "Technical guides, diagrams, and architecture walkthroughs for learning Crab progressively.",
  path: "/blog",
})

const CATEGORIES: BlogPostMeta["category"][] = [
  "Product",
  "Tutorial",
  "Architecture",
  "Use Case",
  "Release",
]

const pathIcons: Record<BlogLearningPathKey, typeof BookOpen> = {
  "start-here": BookOpen,
  "first-workflow": GitBranch,
  "core-internals": Network,
  "advanced-operations": Layers,
}

const SEQUENCE_COLUMN_COUNT = 4
const PATH_PREVIEW_COUNT = 2

const categoryIcons: Record<BlogPostMeta["category"], typeof Package> = {
  Product: Package,
  Tutorial: BookOpen,
  Architecture: Network,
  "Use Case": Briefcase,
  Release: Rocket,
}

export default function BlogIndexPage() {
  const posts = getBlogPosts()
  const firstPost =
    posts.find((post) => post.slug === "what-is-crab") ?? posts[0]
  const conceptGroups = buildConceptGroups(posts)
  const totalReadTime = posts.reduce(
    (minutes, post) => minutes + post.readingTimeMinutes,
    0
  )
  const firstPath = getLearningPath(firstPost.path)

  if (posts.length === 0) {
    return (
      <MarketingLayout>
        <section className="mx-auto max-w-6xl px-6 py-24">
          <h1 className="text-4xl font-bold tracking-tight">Crab Blog</h1>
          <p className="mt-4 text-muted-foreground">
            No blog posts available yet. Check back soon.
          </p>
        </section>
      </MarketingLayout>
    )
  }

  return (
    <MarketingLayout>
      <section className="border-b border-border bg-background">
        <div className="mx-auto grid max-w-6xl gap-8 px-6 pt-20 pb-12 lg:grid-cols-[minmax(0,1fr)_24rem] lg:pt-24 lg:pb-14">
          <div>
            <Badge variant="outline" className="gap-1">
              <Route size={12} />
              Crab learning library
            </Badge>
            <h1 className="mt-4 max-w-3xl text-3xl font-bold tracking-tight sm:text-4xl">
              Learn Crab from mental model to production operations.
            </h1>
            <p className="mt-5 max-w-2xl text-base leading-7 text-muted-foreground">
              A guided curriculum for evaluating serverless large-file Git:
              start with the mental model, run the first workflow, then inspect
              deduplication, hydration, consistency, cleanup, and Git LFS
              migration.
            </p>
            <div className="mt-7 flex flex-wrap gap-3">
              <Link
                href={`/blog/${firstPost.slug}`}
                className="inline-flex items-center gap-2 rounded-md bg-primary px-4 py-2.5 text-sm font-medium text-primary-foreground transition-colors hover:bg-primary-hover"
              >
                Start with the overview
                <ArrowRight size={16} />
              </Link>
              <Link
                href="#all-guides"
                className="inline-flex items-center gap-2 rounded-md border border-border bg-background px-4 py-2.5 text-sm font-medium transition-colors hover:bg-muted"
              >
                <Search size={16} />
                Browse all guides
              </Link>
            </div>
            <div className="mt-7 grid max-w-2xl gap-4 border-t border-border pt-5 sm:grid-cols-3">
              <HeroMetric
                label="Guides"
                value={String(posts.length)}
                detail={`${BLOG_LEARNING_PATHS.length} ordered paths`}
              />
              <HeroMetric
                label="Full route"
                value={`${totalReadTime} min`}
                detail="Mental model to operations"
              />
              <HeroMetric
                label="First step"
                value={firstPath.shortLabel}
                detail={`${firstPost.readingTimeMinutes} min overview`}
              />
            </div>
          </div>

          <aside className="self-start rounded-lg border border-border bg-card p-5 shadow-sm">
            <div className="flex items-center justify-between gap-3">
              <Badge variant="secondary">Recommended first</Badge>
              <span className="text-xs font-medium text-muted-foreground">
                Step {firstPost.pathOrder}
              </span>
            </div>
            <h2 className="mt-4 text-xl leading-tight font-semibold">
              {firstPost.title}
            </h2>
            <p className="mt-3 text-sm leading-6 text-muted-foreground">
              {firstPost.description}
            </p>
            <div className="mt-5 divide-y divide-border text-sm">
              <HeroInfoRow label="Depth" value={firstPost.level} />
              <HeroInfoRow
                label="Read time"
                value={`${firstPost.readingTimeMinutes} min`}
              />
              <HeroInfoRow label="Path" value={firstPath.label} />
              <HeroInfoRow
                label="Diagram"
                value={firstPost.diagramType ?? "Guide"}
              />
            </div>
            <Link
              href={`/blog/${firstPost.slug}`}
              className="mt-5 inline-flex items-center gap-1 text-sm font-medium text-primary transition-colors hover:text-primary-hover"
            >
              Read the guide
              <ArrowRight size={14} />
            </Link>
          </aside>
        </div>
      </section>

      <section className="border-b border-border bg-muted/20">
        <div className="mx-auto max-w-6xl px-6 py-14">
          <Reveal>
            <div className="mb-8 flex flex-col gap-3 sm:flex-row sm:items-end sm:justify-between">
              <div>
                <Badge variant="outline" className="gap-1">
                  <Route size={12} />
                  Follow the sequence
                </Badge>
                <h2 className="mt-4 text-2xl font-semibold tracking-tight">
                  Follow the sequence
                </h2>
                <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
                  Each step has a job: orient, try, inspect, then operate.
                  Migration now lives with the operational material because it
                  depends on the same storage, cost, and consistency concepts.
                </p>
              </div>
              <div className="rounded-md border border-border bg-background px-3 py-2 text-sm text-muted-foreground">
                <span className="font-medium text-foreground">
                  {posts.length}
                </span>{" "}
                guides across{" "}
                <span className="font-medium text-foreground">
                  {BLOG_LEARNING_PATHS.length}
                </span>{" "}
                paths
              </div>
            </div>
          </Reveal>

          <ol className="grid gap-4 lg:grid-cols-4">
            {BLOG_LEARNING_PATHS.map((path, index) => (
              <LearningPathCard
                key={path.key}
                path={path}
                posts={getPathPosts(path.key, posts)}
                hasConnector={
                  (index + 1) % SEQUENCE_COLUMN_COUNT !== 0 &&
                  index < BLOG_LEARNING_PATHS.length - 1
                }
              />
            ))}
          </ol>
        </div>
      </section>

      <section>
        <div className="mx-auto grid max-w-6xl gap-8 px-6 py-16 lg:grid-cols-[22rem_minmax(0,1fr)]">
          <div>
            <Badge variant="outline" className="gap-1">
              <Network size={12} />
              Concept map
            </Badge>
            <h2 className="mt-4 text-2xl font-semibold tracking-tight">
              Choose by what you need to understand.
            </h2>
            <p className="mt-3 text-sm leading-6 text-muted-foreground">
              Crab spans Git extension points, object storage, deduplication,
              hydration, and operational safety. These clusters make the system
              easier to navigate before reading deep internals.
            </p>
          </div>

          <div className="grid gap-3 sm:grid-cols-2">
            {conceptGroups.map((group) => {
              const Icon = categoryIcons[group.category]

              return (
                <Link
                  key={group.category}
                  href={`/blog?category=${encodeURIComponent(group.category)}#all-guides`}
                  className="rounded-lg border border-border bg-card p-4 transition-all hover:shadow-card-hover hover:ring-1 hover:ring-primary/20"
                >
                  <div className="flex items-center gap-2">
                    <span className="flex h-8 w-8 items-center justify-center rounded-md bg-primary/10 text-primary">
                      <Icon size={16} />
                    </span>
                    <div>
                      <h3 className="text-sm font-semibold">
                        {group.category}
                      </h3>
                      <p className="text-xs text-muted-foreground">
                        {group.count} guides
                      </p>
                    </div>
                  </div>
                  <div className="mt-4 flex flex-wrap gap-1.5">
                    {group.concepts.map((concept) => (
                      <span
                        key={concept}
                        className="rounded-full border border-border px-2 py-0.5 text-[0.68rem] text-muted-foreground"
                      >
                        {concept}
                      </span>
                    ))}
                  </div>
                </Link>
              )
            })}
          </div>
        </div>
      </section>

      <section
        id="all-guides"
        className="mx-auto max-w-6xl border-t border-border px-6 py-16 pb-24"
      >
        <Suspense
          fallback={
            <div className="min-h-80 rounded-lg border border-border" />
          }
        >
          <BlogIndexContent categories={CATEGORIES} posts={posts} />
        </Suspense>
      </section>
    </MarketingLayout>
  )
}

function LearningPathCard({
  path,
  posts,
  hasConnector,
}: {
  path: BlogLearningPath
  posts: BlogPostMeta[]
  hasConnector: boolean
}) {
  const Icon = pathIcons[path.key]
  const totalMinutes = posts.reduce(
    (minutes, post) => minutes + post.readingTimeMinutes,
    0
  )
  const visiblePosts = posts.slice(0, PATH_PREVIEW_COUNT)
  const hiddenPosts = posts.slice(PATH_PREVIEW_COUNT)
  const hiddenPostCount = posts.length - visiblePosts.length
  const pathHref = getPathFilterHref(path.key)

  return (
    <li
      className={cn(
        "relative",
        hasConnector &&
          "lg:after:absolute lg:after:top-6 lg:after:left-[calc(100%+0.25rem)] lg:after:h-px lg:after:w-3 lg:after:bg-border"
      )}
    >
      <div className="flex h-full flex-col rounded-lg border border-border bg-card p-4 shadow-sm transition-all hover:shadow-card-hover hover:ring-1 hover:ring-primary/20">
        <div className="flex items-start justify-between gap-3">
          <span className="flex h-10 w-10 items-center justify-center rounded-md bg-primary/10 text-primary">
            <Icon size={18} aria-hidden="true" />
          </span>
          <span className="rounded-full bg-muted px-2 py-1 text-[0.68rem] font-medium tracking-wide text-muted-foreground uppercase">
            Step {path.order}
          </span>
        </div>

        <h3 className="mt-4 text-base leading-snug font-semibold">
          {path.label}
        </h3>
        <p className="mt-2 line-clamp-3 text-xs leading-5 text-muted-foreground">
          {path.description}
        </p>

        <div className="mt-4 grid grid-cols-2 gap-3 border-y border-border py-3">
          <PathMetric label="Guides" value={String(posts.length)} />
          <PathMetric label="Read" value={`${totalMinutes}m`} />
        </div>

        <div className="mt-3 text-xs">
          <div className="text-[0.68rem] font-semibold tracking-wide text-muted-foreground uppercase">
            For
          </div>
          <div className="mt-1 font-medium text-foreground">
            {path.audience}
          </div>
        </div>

        <div className="mt-4">
          <div className="text-[0.68rem] font-semibold tracking-wide text-muted-foreground uppercase">
            Guides
          </div>
          <div className="mt-2 space-y-1.5">
            {visiblePosts.map((post) => (
              <PathPostLink key={post.slug} post={post} />
            ))}
            {hiddenPostCount > 0 && (
              <details className="group rounded-md">
                <summary className="cursor-pointer list-none rounded-md px-1.5 py-1 text-xs font-medium text-muted-foreground transition-colors hover:bg-muted hover:text-primary focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none [&::-webkit-details-marker]:hidden">
                  <span className="group-open:hidden">
                    +{hiddenPostCount} more guide
                    {hiddenPostCount === 1 ? "" : "s"}
                  </span>
                  <span className="hidden group-open:inline">
                    Hide extra guides
                  </span>
                </summary>
                <div className="mt-1 space-y-1.5">
                  {hiddenPosts.map((post) => (
                    <PathPostLink key={post.slug} post={post} />
                  ))}
                </div>
              </details>
            )}
          </div>
        </div>

        <Link
          href={pathHref}
          className="mt-auto inline-flex items-center gap-1 pt-4 text-xs font-medium text-primary transition-colors hover:text-primary-hover"
        >
          View full path
          <ArrowRight size={12} />
        </Link>
      </div>
    </li>
  )
}

function PathPostLink({ post }: { post: BlogPostMeta }) {
  return (
    <Link
      href={`/blog/${post.slug}`}
      className="group flex min-w-0 gap-2 rounded-md p-1.5 transition-colors hover:bg-muted"
    >
      <CheckCircle2
        size={14}
        className="mt-0.5 shrink-0 text-primary"
        aria-hidden="true"
      />
      <span className="min-w-0 text-xs leading-5">
        <span className="line-clamp-2 font-medium break-words group-hover:text-primary">
          {post.title}
        </span>
        <span className="mt-0.5 flex items-center gap-1 text-muted-foreground">
          <Clock size={11} />
          {post.readingTimeMinutes} min
        </span>
      </span>
    </Link>
  )
}

function getPathFilterHref(path: BlogLearningPathKey) {
  return `/blog?path=${encodeURIComponent(path)}#all-guides`
}

function HeroMetric({
  label,
  value,
  detail,
}: {
  label: string
  value: string
  detail: string
}) {
  return (
    <div>
      <div className="text-[0.68rem] font-medium tracking-wide text-muted-foreground uppercase">
        {label}
      </div>
      <div className="mt-1 text-xl font-semibold tracking-tight text-foreground">
        {value}
      </div>
      <div className="mt-1 text-xs leading-5 text-muted-foreground">
        {detail}
      </div>
    </div>
  )
}

function HeroInfoRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between gap-4 py-2 first:pt-0 last:pb-0">
      <span className="text-muted-foreground">{label}</span>
      <span className="text-right font-medium text-foreground">{value}</span>
    </div>
  )
}

function PathMetric({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <div className="text-[0.68rem] font-semibold tracking-wide text-muted-foreground uppercase">
        {label}
      </div>
      <div className="mt-1 text-sm font-semibold text-foreground">{value}</div>
    </div>
  )
}

function buildConceptGroups(posts: BlogPostMeta[]) {
  return CATEGORIES.map((category) => {
    const categoryPosts = posts.filter((post) => post.category === category)
    const concepts = Array.from(
      new Set(categoryPosts.flatMap((post) => post.concepts))
    ).slice(0, 6)

    return {
      category,
      count: categoryPosts.length,
      concepts,
    }
  }).filter((group) => group.count > 0)
}
