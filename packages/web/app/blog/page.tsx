import Link from "next/link"
import { Suspense } from "react"
import {
  ArrowRight,
  BookOpen,
  Briefcase,
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
                  Move from orientation to hands-on work, then into internals
                  and operations. Every guide is shown in reading order, so you
                  can see the complete route before you begin.
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

          <ol>
            {BLOG_LEARNING_PATHS.map((path, index) => (
              <LearningPathStep
                key={path.key}
                path={path}
                posts={getPathPosts(path.key, posts)}
                isLast={index === BLOG_LEARNING_PATHS.length - 1}
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

function LearningPathStep({
  path,
  posts,
  isLast,
}: {
  path: BlogLearningPath
  posts: BlogPostMeta[]
  isLast: boolean
}) {
  const Icon = pathIcons[path.key]
  const totalMinutes = posts.reduce(
    (minutes, post) => minutes + post.readingTimeMinutes,
    0
  )
  const pathHref = getPathFilterHref(path.key)

  return (
    <li className="grid grid-cols-[2.5rem_minmax(0,1fr)] gap-3 pb-5 last:pb-0 sm:grid-cols-[3rem_minmax(0,1fr)] sm:gap-5">
      <div className="flex flex-col items-center" aria-hidden="true">
        <span className="flex h-10 w-10 shrink-0 items-center justify-center rounded-full border border-primary/30 bg-background text-sm font-semibold text-primary shadow-sm sm:h-12 sm:w-12">
          {path.order}
        </span>
        {!isLast && <span className="mt-2 w-px flex-1 bg-border" />}
      </div>

      <article className="overflow-hidden rounded-xl border border-border bg-card shadow-sm lg:grid lg:grid-cols-[18rem_minmax(0,1fr)]">
        <div className="border-b border-border bg-muted/20 p-5 sm:p-6 lg:border-r lg:border-b-0">
          <div className="flex items-center justify-between gap-3">
            <span className="flex h-10 w-10 items-center justify-center rounded-md bg-primary/10 text-primary">
              <Icon size={18} aria-hidden="true" />
            </span>
            <span className="text-[0.68rem] font-semibold tracking-wide text-muted-foreground uppercase">
              Step {path.order} of {BLOG_LEARNING_PATHS.length}
            </span>
          </div>

          <h3 className="mt-5 text-xl leading-tight font-semibold">
            {path.label}
          </h3>
          <p className="mt-2 text-sm leading-6 text-muted-foreground">
            {path.description}
          </p>

          <div className="mt-5 grid grid-cols-2 gap-4 border-y border-border py-4">
            <PathMetric label="Guides" value={String(posts.length)} />
            <PathMetric label="Read time" value={`${totalMinutes} min`} />
          </div>

          <div className="mt-4 text-[0.68rem] font-semibold tracking-wide text-muted-foreground uppercase">
            Best for
          </div>
          <div className="mt-1 text-sm font-medium text-foreground">
            {path.audience}
          </div>

          <Link
            href={pathHref}
            className="mt-5 inline-flex items-center gap-1 text-sm font-medium text-primary transition-colors hover:text-primary-hover focus-visible:rounded-sm focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none"
          >
            Explore this path
            <ArrowRight size={14} aria-hidden="true" />
          </Link>
        </div>

        <div className="min-w-0">
          <div className="border-b border-border px-5 py-4 sm:flex sm:items-center sm:justify-between sm:gap-4 sm:px-6">
            <div>
              <div className="text-sm font-semibold text-foreground">
                Guides in this step
              </div>
              <p className="mt-1 text-xs text-muted-foreground">
                Read from top to bottom for the intended progression.
              </p>
            </div>
            <span className="mt-2 block text-xs font-medium text-muted-foreground sm:mt-0">
              {posts.length} guide{posts.length === 1 ? "" : "s"}
            </span>
          </div>

          <ol className="divide-y divide-border">
            {posts.map((post, index) => (
              <li key={post.slug}>
                <PathPostLink post={post} sequenceNumber={index + 1} />
              </li>
            ))}
          </ol>
        </div>
      </article>
    </li>
  )
}

function PathPostLink({
  post,
  sequenceNumber,
}: {
  post: BlogPostMeta
  sequenceNumber: number
}) {
  return (
    <Link
      href={`/blog/${post.slug}`}
      className="group grid min-w-0 grid-cols-[2rem_minmax(0,1fr)] gap-3 px-5 py-4 transition-colors hover:bg-muted/50 focus-visible:bg-muted/50 focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none focus-visible:ring-inset sm:grid-cols-[2rem_minmax(0,1fr)_auto] sm:items-center sm:px-6"
    >
      <span className="flex h-8 w-8 items-center justify-center rounded-full bg-primary/10 text-xs font-semibold text-primary">
        {sequenceNumber}
      </span>
      <span className="min-w-0">
        <span className="block text-sm leading-5 font-semibold text-foreground transition-colors group-hover:text-primary">
          {post.title}
        </span>
        <span className="mt-1 block text-xs leading-5 text-muted-foreground">
          {post.description}
        </span>
        <span className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[0.68rem] font-medium text-muted-foreground sm:hidden">
          <span>{post.level}</span>
          <span className="flex items-center gap-1">
            <Clock size={11} aria-hidden="true" />
            {post.readingTimeMinutes} min
          </span>
        </span>
      </span>
      <span className="hidden items-center gap-4 pl-4 text-xs text-muted-foreground sm:flex">
        <span>{post.level}</span>
        <span className="flex items-center gap-1 whitespace-nowrap">
          <Clock size={12} aria-hidden="true" />
          {post.readingTimeMinutes} min
        </span>
        <ArrowRight
          size={14}
          className="text-primary transition-transform group-hover:translate-x-0.5"
          aria-hidden="true"
        />
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
