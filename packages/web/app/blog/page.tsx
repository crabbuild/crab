import {
  ArrowRight,
  Clock,
  Cloud,
  GitCommit,
  Gauge,
  Newspaper,
  Users,
} from "lucide-react"
import Link from "next/link"

import { MarketingLayout } from "@/components/marketing-layout"
import { Badge } from "@/components/ui/badge"
import { formatBlogDate } from "@/lib/blog-date"
import { getBlogPosts, type BlogPostMeta } from "@/lib/blog-posts"
import { createPageMetadata } from "@/lib/metadata"

export const metadata = createPageMetadata({
  title: "Crab Blog",
  description:
    "Engineering notes and product thinking from the team building Crab.",
  path: "/blog",
})

export default function BlogDashboardPage() {
  const posts = getBlogPosts()
  const featuredPost = posts[0]
  const remainingPosts = posts.slice(1)

  return (
    <MarketingLayout>
      <section className="border-b border-border bg-background text-foreground">
        <div className="mx-auto grid max-w-6xl gap-8 px-6 pt-20 pb-14 lg:grid-cols-[minmax(0,1fr)_23rem] lg:pt-24 lg:pb-16">
          <div>
            <Badge variant="outline" className="gap-1 bg-background">
              <Newspaper className="size-3" aria-hidden="true" />
              Crab blog
            </Badge>
            <h1 className="mt-5 max-w-3xl text-4xl font-black tracking-[-0.045em] sm:text-5xl">
              Notes from building Git for large files.
            </h1>
            <p className="mt-5 max-w-2xl text-base leading-7 text-muted-foreground">
              Product decisions, system boundaries, and lessons from making
              object storage behave like a dependable Git remote.
            </p>
            <Link
              href="/library"
              className="mt-7 inline-flex min-h-11 items-center gap-2 rounded-lg bg-foreground px-4 py-2 text-sm font-bold text-background transition-colors hover:bg-foreground/90 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:outline-none"
            >
              Browse learning materials
              <ArrowRight className="size-4" aria-hidden="true" />
            </Link>
          </div>

          <CurrentSubjectDiagram />
        </div>
      </section>

      <main className="mx-auto max-w-6xl px-6 py-14 sm:py-16">
        <div className="flex flex-col gap-3 border-b border-border pb-5 sm:flex-row sm:items-end sm:justify-between">
          <div>
            <p className="font-mono text-[10px] font-black tracking-[0.18em] text-primary">
              EDITORIAL LEDGER
            </p>
            <h2 className="mt-2 text-2xl font-bold tracking-tight">
              Published notes
            </h2>
          </div>
          <p className="text-sm text-muted-foreground">
            {posts.length} article{posts.length === 1 ? "" : "s"} · newest first
          </p>
        </div>

        {!featuredPost ? (
          <div className="border-b border-border py-12">
            <h3 className="text-lg font-bold">No notes published yet.</h3>
            <p className="mt-2 text-sm text-muted-foreground">
              Add an MDX file to the blog collection to publish the first note.
            </p>
          </div>
        ) : (
          <>
            <FeaturedPost post={featuredPost} />
            {remainingPosts.length > 0 && (
              <section className="mt-12" aria-labelledby="more-notes">
                <h2 id="more-notes" className="text-lg font-bold">
                  More engineering notes
                </h2>
                <div className="mt-5 grid gap-5 md:grid-cols-2">
                  {remainingPosts.map((post) => (
                    <PostCard key={post.slug} post={post} />
                  ))}
                </div>
              </section>
            )}
          </>
        )}
      </main>
    </MarketingLayout>
  )
}

function FeaturedPost({ post }: { post: BlogPostMeta }) {
  return (
    <Link
      href={`/blog/${post.slug}`}
      className="group grid border-b border-border py-8 outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-4 focus-visible:outline-none md:grid-cols-[9rem_minmax(0,1fr)_13rem] md:items-start"
    >
      <div className="font-mono text-xs font-black text-muted-foreground">
        <span className="block text-3xl tracking-[-0.05em] text-foreground">
          {new Date(post.date).getUTCDate().toString().padStart(2, "0")}
        </span>
        {formatBlogDate(post.date, "short")}
      </div>

      <div className="mt-5 md:mt-0">
        <div className="flex flex-wrap items-center gap-2">
          <Badge variant="secondary">{post.category}</Badge>
          <span className="font-mono text-[10px] font-black tracking-[0.14em] text-emerald-700 dark:text-emerald-300">
            FEATURED
          </span>
        </div>
        <h3 className="mt-3 max-w-3xl text-2xl font-bold tracking-tight transition-colors group-hover:text-primary sm:text-3xl">
          {post.title}
        </h3>
        <p className="mt-3 max-w-3xl text-sm leading-6 text-muted-foreground">
          {post.excerpt}
        </p>
        <TagList tags={post.tags} />
      </div>

      <div className="mt-6 grid gap-3 border-l-0 border-border text-xs md:mt-0 md:border-l md:pl-6">
        <MetaLine icon={Clock} label={`${post.readingTimeMinutes} min read`} />
        <MetaLine icon={Gauge} label={post.level} />
        <MetaLine icon={Users} label={post.audience} />
        <span className="mt-2 inline-flex min-h-11 items-center gap-2 font-bold text-primary">
          Read article
          <ArrowRight
            className="size-4 transition-transform group-hover:translate-x-1"
            aria-hidden="true"
          />
        </span>
      </div>
    </Link>
  )
}

function PostCard({ post }: { post: BlogPostMeta }) {
  return (
    <Link
      href={`/blog/${post.slug}`}
      className="group flex h-full flex-col rounded-xl border border-border bg-card p-5 transition-[border-color,box-shadow] outline-none hover:border-primary/30 hover:shadow-sm focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:outline-none"
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <Badge variant="secondary">{post.category}</Badge>
        <time className="text-xs text-muted-foreground" dateTime={post.date}>
          {formatBlogDate(post.date, "short")}
        </time>
      </div>
      <h3 className="mt-4 text-xl font-bold tracking-tight transition-colors group-hover:text-primary">
        {post.title}
      </h3>
      <p className="mt-3 text-sm leading-6 text-muted-foreground">
        {post.excerpt}
      </p>
      <TagList tags={post.tags} />
      <div className="mt-auto grid gap-2 border-t border-border pt-4 text-xs">
        <MetaLine icon={Clock} label={`${post.readingTimeMinutes} min read`} />
        <MetaLine icon={Gauge} label={post.level} />
        <MetaLine icon={Users} label={post.audience} />
      </div>
    </Link>
  )
}

function TagList({ tags }: { tags: string[] }) {
  return (
    <div className="my-5 flex flex-wrap gap-1.5">
      {tags.map((tag) => (
        <span
          key={tag}
          className="rounded-full border border-border px-2 py-0.5 text-[10px] font-medium text-muted-foreground"
        >
          {tag}
        </span>
      ))}
    </div>
  )
}

function MetaLine({
  icon: Icon,
  label,
}: {
  icon: typeof Clock
  label: string
}) {
  return (
    <span className="flex items-start gap-2 text-muted-foreground">
      <Icon
        className="mt-0.5 size-3.5 shrink-0 text-primary"
        aria-hidden="true"
      />
      <span className="leading-5">{label}</span>
    </span>
  )
}

function CurrentSubjectDiagram() {
  return (
    <aside className="self-start rounded-xl border border-border bg-card p-5 shadow-sm">
      <div className="flex items-center justify-between">
        <span className="font-mono text-[9px] font-black tracking-[0.17em]">
          CURRENT SUBJECT
        </span>
        <span
          className="size-2 rounded-full bg-emerald-500"
          aria-hidden="true"
        />
      </div>
      <div className="mt-4">
        <div className="flex items-center gap-2 rounded-md bg-muted px-3 py-2 font-mono text-[10px] font-black text-foreground">
          <span
            className="size-1.5 rounded-full bg-primary"
            aria-hidden="true"
          />
          ONE COMMIT
        </div>
        <div className="mx-auto h-5 w-px bg-border" aria-hidden="true" />
        <div className="grid grid-cols-2 gap-3">
          <DiagramLane
            icon={GitCommit}
            label="Git history"
            value="code + pointer"
            colorClassName="text-blue-600 dark:text-blue-400"
          />
          <DiagramLane
            icon={Cloud}
            label="Object store"
            value="large bytes"
            colorClassName="text-orange-600 dark:text-orange-400"
          />
        </div>
        <p className="m-0 mt-4 border-t border-border pt-3 text-center text-xs font-bold text-emerald-700 dark:text-emerald-300">
          One visible repository state
        </p>
      </div>
    </aside>
  )
}

function DiagramLane({
  icon: Icon,
  label,
  value,
  colorClassName,
}: {
  icon: typeof GitCommit
  label: string
  value: string
  colorClassName: string
}) {
  return (
    <div className="rounded-lg border border-border bg-muted/20 p-3">
      <Icon className={`size-4 ${colorClassName}`} aria-hidden="true" />
      <p className="m-0 mt-3 text-[10px] font-black tracking-wide uppercase">
        {label}
      </p>
      <p className="m-0 mt-1 text-[10px] leading-4 text-muted-foreground">
        {value}
      </p>
    </div>
  )
}
