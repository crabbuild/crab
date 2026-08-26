import type { Metadata } from "next"
import {
  ArrowLeft,
  ArrowRight,
  BookOpen,
  Clock,
  Gauge,
  Tag,
  Users,
} from "lucide-react"
import Link from "next/link"
import { notFound } from "next/navigation"
import { isValidElement, type ReactElement, type ReactNode } from "react"

import { FeatureBlogArticle } from "@/components/blog/feature-blog-article"
import { MarketingLayout } from "@/components/marketing-layout"
import { Badge } from "@/components/ui/badge"
import { formatBlogDate } from "@/lib/blog-date"
import { getBlogPost, type BlogPostMeta } from "@/lib/blog-posts"
import { blogSource } from "@/lib/blog-source"
import { createPageMetadata } from "@/lib/metadata"
import { getMDXComponents } from "@/mdx-components"

export function generateStaticParams() {
  return blogSource.getPages().map((page) => ({
    slug: page.slugs[0],
  }))
}

export async function generateMetadata({
  params,
}: {
  params: Promise<{ slug: string }>
}): Promise<Metadata> {
  const { slug } = await params
  const page = blogSource.getPage([slug])
  const post = getBlogPost(slug)
  if (!page || !post) return {}

  return createPageMetadata({
    title: `${post.title} — Crab Blog`,
    description: post.description,
    path: `/blog/${slug}`,
    absoluteTitle: true,
    image: {
      openGraph: `/blog/${slug}/opengraph-image`,
      twitter: `/blog/${slug}/twitter-image`,
      alt: `${post.title} — Crab Blog`,
    },
    article: {
      publishedTime: new Date(post.date).toISOString(),
      authors: [post.author],
      tags: post.tags,
    },
  })
}

export default async function BlogPostPage({
  params,
}: {
  params: Promise<{ slug: string }>
}) {
  const { slug } = await params
  const page = blogSource.getPage([slug])
  const post = getBlogPost(slug)
  if (!page || !post) notFound()

  const MDX = page.data.body

  if (page.data.presentation === "feature") {
    const toc = page.data.toc.map((item) => ({
      title: tocTitleToString(item.title),
      url: item.url,
      depth: item.depth,
    }))

    return (
      <FeatureBlogArticle post={post} toc={toc}>
        <MDX components={getMDXComponents({})} />
      </FeatureBlogArticle>
    )
  }

  return (
    <MarketingLayout>
      <article>
        <header className="border-b border-border bg-background text-foreground">
          <div className="mx-auto max-w-6xl px-6 pt-24 pb-12 lg:pt-28 lg:pb-16">
            <Link
              href="/blog"
              className="inline-flex min-h-11 items-center gap-2 text-sm font-medium text-muted-foreground transition-colors hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none"
            >
              <ArrowLeft className="size-4" aria-hidden="true" />
              Blog dashboard
            </Link>

            <div className="mt-7 grid gap-8 lg:grid-cols-[minmax(0,1fr)_20rem]">
              <div>
                <div className="flex flex-wrap items-center gap-2">
                  <Badge variant="secondary">{post.category}</Badge>
                  <Badge variant="outline" className="bg-background">
                    {post.level}
                  </Badge>
                  <time
                    className="text-xs text-muted-foreground"
                    dateTime={post.date}
                  >
                    {formatBlogDate(post.date)}
                  </time>
                </div>
                <h1 className="mt-5 max-w-4xl text-4xl font-black tracking-[-0.045em] sm:text-5xl lg:text-6xl">
                  {post.title}
                </h1>
                <p className="mt-5 max-w-3xl text-lg leading-8 text-muted-foreground">
                  {post.description}
                </p>
                <p className="mt-6 text-sm font-medium text-muted-foreground">
                  {post.author}
                </p>
              </div>

              <ReaderContract post={post} />
            </div>
          </div>
        </header>

        <div className="mx-auto max-w-7xl px-4 py-12 sm:px-6 lg:px-8 lg:py-16">
          <div className="prose-neutral dark:prose-invert mx-auto prose max-w-[46rem] prose-headings:scroll-mt-24">
            <MDX components={getMDXComponents({})} />
          </div>
        </div>
      </article>

      <section className="border-t border-border bg-background text-foreground">
        <div className="mx-auto flex max-w-5xl flex-col gap-5 px-6 py-10 sm:flex-row sm:items-center sm:justify-between">
          <div>
            <p className="font-mono text-[10px] font-black tracking-[0.18em] text-primary">
              CONTINUE LEARNING
            </p>
            <h2 className="mt-2 text-xl font-bold">
              Turn the article into working knowledge.
            </h2>
          </div>
          <Link
            href="/library"
            className="inline-flex min-h-11 w-fit items-center gap-2 rounded-lg bg-foreground px-4 py-2 text-sm font-bold text-background hover:bg-foreground/90 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:outline-none"
          >
            <BookOpen className="size-4" aria-hidden="true" />
            Open the Library
            <ArrowRight className="size-4" aria-hidden="true" />
          </Link>
        </div>
      </section>
    </MarketingLayout>
  )
}

function tocTitleToString(node: ReactNode): string {
  if (typeof node === "string" || typeof node === "number") {
    return String(node)
  }
  if (Array.isArray(node)) return node.map(tocTitleToString).join("")
  if (!isValidElement(node)) return ""

  return tocTitleToString(
    (node as ReactElement<{ children?: ReactNode }>).props.children
  )
}

function ReaderContract({ post }: { post: BlogPostMeta }) {
  return (
    <aside className="self-start rounded-xl border border-border bg-card p-4 shadow-sm">
      <div className="font-mono text-[9px] font-black tracking-[0.17em]">
        READER CONTRACT
      </div>
      <div className="mt-4 grid gap-3 text-xs text-muted-foreground">
        <Attribute
          icon={Clock}
          label="Reading time"
          value={`${post.readingTimeMinutes} min`}
        />
        <Attribute icon={Gauge} label="Depth" value={post.level} />
        <Attribute icon={Users} label="For" value={post.audience} />
      </div>
      <div className="mt-4 border-t border-border pt-4">
        <div className="flex items-center gap-2 font-mono text-[9px] font-black tracking-[0.14em] text-muted-foreground">
          <Tag className="size-3.5" aria-hidden="true" />
          TAGS
        </div>
        <div className="mt-3 flex flex-wrap gap-1.5">
          {post.tags.map((tag) => (
            <span
              key={tag}
              className="rounded-full border border-border px-2 py-0.5 text-[10px] text-muted-foreground"
            >
              {tag}
            </span>
          ))}
        </div>
      </div>
    </aside>
  )
}

function Attribute({
  icon: Icon,
  label,
  value,
}: {
  icon: typeof Clock
  label: string
  value: string
}) {
  return (
    <div className="grid grid-cols-[1rem_5rem_minmax(0,1fr)] items-start gap-2">
      <Icon className="mt-0.5 size-3.5 text-primary" aria-hidden="true" />
      <span>{label}</span>
      <span className="font-bold text-foreground">{value}</span>
    </div>
  )
}
