import type { Metadata } from "next"
import { notFound } from "next/navigation"
import Link from "next/link"
import {
  ArrowLeft,
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
  Tag,
} from "lucide-react"

import { MarketingLayout } from "@/components/marketing-layout"
import { KnowledgeCheck } from "@/components/library/knowledge-check"
import { Reveal } from "@/components/marketing/reveal"
import { Badge } from "@/components/ui/badge"
import { Card, CardHeader, CardTitle } from "@/components/ui/card"
import {
  getAdjacentPathGuides,
  getLibraryPath,
  getPathGuides,
  getRelatedGuides,
  type LibraryPathKey,
  type LibraryGuideMeta,
} from "@/lib/library"
import { formatBlogDate } from "@/lib/blog-date"
import { librarySource } from "@/lib/library-source"
import { getLibraryGuide, getLibraryGuides } from "@/lib/library-guides"
import { createPageMetadata } from "@/lib/metadata"
import { cn } from "@/lib/utils"
import { getMDXComponents } from "@/mdx-components"

const categoryIcons: Record<LibraryGuideMeta["category"], typeof Package> = {
  Product: Package,
  Tutorial: BookOpen,
  Architecture: Network,
  "Use Case": Briefcase,
  Release: Rocket,
}

const pathIcons: Record<LibraryPathKey, typeof BookOpen> = {
  "start-here": BookOpen,
  "first-workflow": GitBranch,
  "core-internals": Network,
  "advanced-operations": Layers,
}

export function generateStaticParams() {
  return librarySource.getPages().map((page) => ({
    slug: page.slugs[0],
  }))
}

export async function generateMetadata({
  params,
}: {
  params: Promise<{ slug: string }>
}): Promise<Metadata> {
  const { slug } = await params
  const page = librarySource.getPage([slug])
  if (!page) return {}

  const { title, description } = page.data
  const post = getLibraryGuide(slug)

  return createPageMetadata({
    title: title ? `${title} — Crab Library` : "Crab Library",
    description:
      description ?? "Interactive learning guides from the Crab team.",
    path: `/library/${slug}`,
    absoluteTitle: true,
    article: post
      ? {
          publishedTime: new Date(post.date).toISOString(),
          authors: [post.author.name],
          tags: post.tags,
        }
      : undefined,
  })
}

export default async function LibraryGuidePage({
  params,
}: {
  params: Promise<{ slug: string }>
}) {
  const { slug } = await params
  const page = librarySource.getPage([slug])
  const currentPost = getLibraryGuide(slug)

  if (!page || !currentPost) notFound()

  const MDX = page.data.body
  const allPosts = getLibraryGuides()
  const learningPath = getLibraryPath(currentPost.path)
  const pathPosts = getPathGuides(currentPost.path, allPosts)
  const adjacentPosts = getAdjacentPathGuides(currentPost, allPosts)
  const relatedPosts = getRelatedGuides(currentPost, allPosts)
  const CategoryIcon = categoryIcons[currentPost.category] ?? Package
  const PathIcon = pathIcons[currentPost.path] ?? BookOpen

  return (
    <MarketingLayout>
      <article className="border-b border-border">
        <section className="mx-auto max-w-6xl px-4 pt-24 pb-10 sm:px-6 lg:px-8 lg:pt-28">
          <nav className="mb-8">
            <Link
              href="/library"
              className="inline-flex items-center gap-1.5 text-sm text-muted-foreground transition-colors hover:text-foreground"
            >
              <ArrowLeft size={14} />
              All guides
            </Link>
          </nav>

          <div className="grid gap-8 lg:grid-cols-[minmax(0,1fr)_20rem]">
            <header>
              <div className="flex flex-wrap items-center gap-2">
                <Link href={getPathFilterHref(currentPost.path)}>
                  <Badge variant="secondary" className="gap-1">
                    <PathIcon size={12} />
                    {learningPath.label} {currentPost.pathOrder}
                  </Badge>
                </Link>
                <Link
                  href={`/library?category=${encodeURIComponent(currentPost.category)}`}
                >
                  <Badge variant="outline" className="gap-1">
                    <CategoryIcon size={12} />
                    {currentPost.category}
                  </Badge>
                </Link>
                <Badge variant="outline">{currentPost.level}</Badge>
                <span className="inline-flex items-center gap-1 text-xs text-muted-foreground">
                  <Clock size={12} />
                  {currentPost.readingTimeMinutes} min read
                </span>
              </div>

              <h1 className="mt-5 max-w-4xl text-4xl font-bold tracking-tight sm:text-5xl">
                {currentPost.title}
              </h1>

              <p className="mt-5 max-w-3xl text-lg leading-8 text-muted-foreground">
                {currentPost.description}
              </p>

              <div className="mt-6 flex flex-wrap items-center gap-3 text-sm text-muted-foreground">
                <time dateTime={currentPost.date}>
                  {formatBlogDate(currentPost.date)}
                </time>
                <span aria-hidden="true">·</span>
                <span>{currentPost.author.name}</span>
              </div>

              {currentPost.tags.length > 0 && (
                <div className="mt-5 flex flex-wrap items-center gap-1.5">
                  <Tag size={14} className="text-muted-foreground" />
                  {currentPost.tags.map((tag) => (
                    <Link
                      key={tag}
                      href={`/library?tag=${encodeURIComponent(tag)}`}
                      className="rounded-full border border-border px-2.5 py-0.5 text-xs text-muted-foreground transition-colors hover:border-primary/50 hover:text-primary"
                    >
                      {tag}
                    </Link>
                  ))}
                </div>
              )}
            </header>

            <aside className="rounded-lg border border-border bg-card p-4 shadow-sm">
              <div className="text-xs font-semibold tracking-wide text-muted-foreground uppercase">
                What you will understand
              </div>
              <p className="mt-2 text-sm leading-6 text-foreground">
                {currentPost.outcome}
              </p>

              <div className="mt-5 grid gap-3 text-xs">
                <InfoRow
                  label="Diagram"
                  value={currentPost.diagramType ?? "Guide"}
                />
                <InfoRow label="Path" value={learningPath.label} />
                <InfoRow label="Audience" value={learningPath.audience} />
                <InfoRow label="Proof" value="1 knowledge check" />
              </div>

              {currentPost.prerequisites.length > 0 && (
                <div className="mt-5">
                  <div className="text-xs font-semibold tracking-wide text-muted-foreground uppercase">
                    Read first
                  </div>
                  <ul className="mt-2 space-y-2">
                    {currentPost.prerequisites.map((item) => (
                      <li
                        key={item}
                        className="flex gap-2 text-sm text-muted-foreground"
                      >
                        <CheckCircle2
                          size={14}
                          className="mt-0.5 shrink-0 text-primary"
                          aria-hidden="true"
                        />
                        {item}
                      </li>
                    ))}
                  </ul>
                </div>
              )}
            </aside>
          </div>
        </section>

        <section className="mx-auto grid max-w-6xl gap-10 px-4 pb-16 has-[.wide-article-visual]:max-w-7xl sm:px-6 lg:grid-cols-[minmax(0,44rem)_18rem] lg:px-8 lg:has-[.wide-article-visual]:grid-cols-[minmax(0,1fr)_18rem]">
          <div className="min-w-0">
            <div className="prose-neutral dark:prose-invert mx-auto prose max-w-[44rem] prose-headings:scroll-mt-24 prose-img:rounded-lg">
              <MDX components={getMDXComponents({})} />
            </div>
            <div className="mx-auto max-w-[44rem]">
              <KnowledgeCheck
                slug={currentPost.slug}
                check={currentPost.knowledgeCheck}
              />
            </div>
          </div>

          <aside className="hidden lg:block">
            <div className="sticky top-24 space-y-6">
              <LearningRail posts={pathPosts} currentPost={currentPost} />
              <ConceptPanel post={currentPost} />
            </div>
          </aside>
        </section>
      </article>

      <section className="mx-auto max-w-6xl px-4 py-12 sm:px-6 lg:px-8">
        <Reveal>
          <div className="grid gap-4 md:grid-cols-2">
            <NextStepCard
              label="Previous in path"
              post={adjacentPosts.previous}
              fallbackHref={`/library?path=${currentPost.path}`}
              fallbackTitle={`Browse ${learningPath.label}`}
            />
            <NextStepCard
              label="Next in path"
              post={adjacentPosts.next}
              fallbackHref="/library"
              fallbackTitle="Explore all guides"
            />
          </div>
        </Reveal>

        {relatedPosts.length > 0 && (
          <Reveal>
            <section className="mt-12" aria-label="Related guides">
              <h2 className="text-lg font-semibold text-foreground">
                Related guides
              </h2>
              <div className="mt-4 grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
                {relatedPosts.map((post) => {
                  const Icon = categoryIcons[post.category] ?? Package

                  return (
                    <Link key={post.slug} href={`/library/${post.slug}`}>
                      <Card
                        size="sm"
                        className="h-full transition-all duration-(--duration-normal) hover:shadow-md hover:ring-1 hover:ring-primary/20"
                      >
                        <CardHeader>
                          <Badge variant="secondary" className="w-fit gap-1">
                            <Icon size={12} />
                            {post.level}
                          </Badge>
                          <CardTitle className="mt-2 line-clamp-2 text-sm font-medium">
                            {post.title}
                          </CardTitle>
                        </CardHeader>
                      </Card>
                    </Link>
                  )
                })}
              </div>
            </section>
          </Reveal>
        )}
      </section>
    </MarketingLayout>
  )
}

function getPathFilterHref(path: LibraryPathKey) {
  return `/library?path=${encodeURIComponent(path)}#all-guides`
}

function InfoRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between gap-3 rounded-md bg-muted/50 px-3 py-2">
      <span className="text-muted-foreground">{label}</span>
      <span className="text-right font-medium text-foreground">{value}</span>
    </div>
  )
}

function LearningRail({
  posts,
  currentPost,
}: {
  posts: LibraryGuideMeta[]
  currentPost: LibraryGuideMeta
}) {
  const learningPath = getLibraryPath(currentPost.path)

  return (
    <nav
      aria-label={`${learningPath.label} learning path`}
      className="rounded-lg border border-border bg-card p-4 shadow-sm"
    >
      <div className="text-xs font-semibold tracking-wide text-muted-foreground uppercase">
        {learningPath.label}
      </div>
      <div className="mt-3 space-y-1">
        {posts.map((post) => {
          const isCurrent = post.slug === currentPost.slug

          return (
            <Link
              key={post.slug}
              href={`/library/${post.slug}`}
              aria-current={isCurrent ? "page" : undefined}
              className={cn(
                "block rounded-md px-3 py-2 text-xs leading-5 transition-colors",
                isCurrent
                  ? "bg-primary/10 font-medium text-primary"
                  : "text-muted-foreground hover:bg-muted hover:text-foreground"
              )}
            >
              <span className="mr-1 font-medium">{post.pathOrder}.</span>
              {post.title}
            </Link>
          )
        })}
      </div>
    </nav>
  )
}

function ConceptPanel({ post }: { post: LibraryGuideMeta }) {
  if (post.concepts.length === 0) return null

  return (
    <section className="rounded-lg border border-border bg-card p-4 shadow-sm">
      <div className="text-xs font-semibold tracking-wide text-muted-foreground uppercase">
        Concepts
      </div>
      <div className="mt-3 flex flex-wrap gap-1.5">
        {post.concepts.map((concept) => (
          <Link
            key={concept}
            href={`/library?tag=${encodeURIComponent(concept)}`}
            className="rounded-full border border-border px-2 py-0.5 text-[0.68rem] text-muted-foreground transition-colors hover:border-primary/50 hover:text-primary"
          >
            {concept}
          </Link>
        ))}
      </div>
    </section>
  )
}

function NextStepCard({
  label,
  post,
  fallbackHref,
  fallbackTitle,
}: {
  label: string
  post?: LibraryGuideMeta
  fallbackHref: string
  fallbackTitle: string
}) {
  const href = post ? `/library/${post.slug}` : fallbackHref
  const title = post?.title ?? fallbackTitle
  const description =
    post?.outcome ?? "Continue through the Crab learning library."

  return (
    <Link
      href={href}
      className="rounded-lg border border-border bg-card p-5 transition-all hover:shadow-card-hover hover:ring-1 hover:ring-primary/20"
    >
      <div className="text-xs font-semibold tracking-wide text-muted-foreground uppercase">
        {label}
      </div>
      <h3 className="mt-2 text-base font-semibold">{title}</h3>
      <p className="mt-2 line-clamp-2 text-sm leading-6 text-muted-foreground">
        {description}
      </p>
      <span className="mt-4 inline-flex items-center gap-1 text-sm font-medium text-primary">
        Continue
        <ArrowRight size={14} />
      </span>
    </Link>
  )
}
