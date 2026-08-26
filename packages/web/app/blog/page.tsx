import {
  ArrowRight,
  BookOpen,
  ChevronDown,
  Cloud,
  GitCommit,
  Newspaper,
} from "lucide-react"
import Link from "next/link"

import { MarketingLayout } from "@/components/marketing-layout"
import { Badge } from "@/components/ui/badge"
import { formatBlogDate } from "@/lib/blog-date"
import { blogSource } from "@/lib/blog-source"
import { createPageMetadata } from "@/lib/metadata"
import { getMDXComponents } from "@/mdx-components"

export const metadata = createPageMetadata({
  title: "Crab Blog",
  description:
    "Engineering notes and product thinking from the team building Crab.",
  path: "/blog",
})

export default function BlogDashboardPage() {
  const posts = [...blogSource.getPages()].sort((a, b) => {
    return (
      new Date(b.data.date ?? 0).getTime() -
      new Date(a.data.date ?? 0).getTime()
    )
  })

  return (
    <MarketingLayout>
      <section className="border-b border-[#b9c7d8] bg-[#f4f7f9] text-[#142033]">
        <div className="mx-auto grid max-w-6xl gap-8 px-6 pt-20 pb-14 lg:grid-cols-[minmax(0,1fr)_23rem] lg:pt-24 lg:pb-16">
          <div>
            <Badge variant="outline" className="gap-1 bg-white">
              <Newspaper className="size-3" aria-hidden="true" />
              Crab blog
            </Badge>
            <h1 className="mt-5 max-w-3xl text-4xl font-black tracking-[-0.045em] sm:text-5xl">
              Notes from building Git for large files.
            </h1>
            <p className="mt-5 max-w-2xl text-base leading-7 text-[#52637a]">
              Product decisions, system boundaries, and lessons from making
              object storage behave like a dependable Git remote.
            </p>
            <Link
              href="/library"
              className="mt-7 inline-flex min-h-11 items-center gap-2 rounded-lg bg-[#163052] px-4 py-2 text-sm font-bold text-white transition-colors hover:bg-[#23466f] focus-visible:ring-2 focus-visible:ring-[#2f6fce] focus-visible:ring-offset-2 focus-visible:outline-none"
            >
              Browse learning materials
              <ArrowRight className="size-4" aria-hidden="true" />
            </Link>
          </div>

          <CurrentSubjectDiagram />
        </div>
      </section>

      <main className="mx-auto max-w-6xl px-6 py-14 sm:py-16">
        <div className="flex flex-col gap-3 border-b-2 border-[#163052] pb-5 sm:flex-row sm:items-end sm:justify-between">
          <div>
            <p className="font-mono text-[10px] font-black tracking-[0.18em] text-[#2f6fce]">
              EDITORIAL LEDGER
            </p>
            <h2 className="mt-2 text-2xl font-bold tracking-tight">
              Published notes
            </h2>
          </div>
          <p className="text-sm text-muted-foreground">
            Newest first · read without leaving the dashboard
          </p>
        </div>

        {posts.length === 0 ? (
          <div className="border-b border-[#b9c7d8] py-12">
            <h3 className="text-lg font-bold">No notes published yet.</h3>
            <p className="mt-2 text-sm text-muted-foreground">
              New engineering notes will appear here when they are published.
            </p>
          </div>
        ) : (
          posts.map((post) => (
            <EditorialEntry key={post.slugs.join("/")} post={post} />
          ))
        )}
      </main>
    </MarketingLayout>
  )
}

type BlogPage = ReturnType<typeof blogSource.getPages>[number]

function EditorialEntry({ post }: { post: BlogPage }) {
  const MDX = post.data.body
  const date = post.data.date ?? "2026-05-01"
  const category = post.data.category
    ? post.data.category.replace("-", " ")
    : "product"

  return (
    <details className="group border-b border-[#b9c7d8]">
      <summary className="grid cursor-pointer list-none gap-5 py-7 outline-none marker:content-none focus-visible:ring-2 focus-visible:ring-[#2f6fce] focus-visible:ring-offset-4 focus-visible:outline-none sm:grid-cols-[8rem_minmax(0,1fr)_auto] sm:items-center">
        <div className="font-mono text-xs font-black text-[#52637a]">
          <span className="block text-2xl tracking-[-0.05em] text-[#142033]">
            {new Date(date).getUTCDate().toString().padStart(2, "0")}
          </span>
          {formatBlogDate(date, "short")}
        </div>

        <div>
          <div className="flex flex-wrap items-center gap-2">
            <Badge variant="secondary" className="capitalize">
              {category}
            </Badge>
            <span className="font-mono text-[10px] font-black tracking-[0.14em] text-[#3d9b72]">
              PUBLISHED
            </span>
          </div>
          <h3 className="mt-3 text-xl font-bold tracking-tight sm:text-2xl">
            {post.data.title}
          </h3>
          <p className="mt-2 max-w-3xl text-sm leading-6 text-muted-foreground">
            {post.data.description}
          </p>
        </div>

        <span className="inline-flex min-h-11 items-center gap-2 text-sm font-bold text-[#2f6fce]">
          Read on this page
          <ChevronDown
            className="size-4 transition-transform group-open:rotate-180"
            aria-hidden="true"
          />
        </span>
      </summary>

      <article className="border-t border-[#b9c7d8] bg-[#f8fafc] px-4 py-10 sm:px-8 lg:px-12 lg:py-14">
        <header className="mx-auto max-w-[46rem] border-b border-[#b9c7d8] pb-7">
          <p className="font-mono text-[10px] font-black tracking-[0.18em] text-[#2f6fce]">
            INTERACTIVE FIELD NOTE
          </p>
          <h2 className="mt-2 text-3xl font-black tracking-[-0.04em]">
            {post.data.title}
          </h2>
          <p className="mt-3 text-sm text-[#607188]">
            {post.data.author ?? "Crab Team"} · {formatBlogDate(date)}
          </p>
        </header>

        <div className="prose-neutral dark:prose-invert mx-auto prose mt-8 max-w-[46rem] prose-headings:scroll-mt-24">
          <MDX components={getMDXComponents({})} />
        </div>

        <div className="mx-auto mt-12 flex max-w-[46rem] flex-col gap-4 border-t-2 border-[#163052] pt-7 sm:flex-row sm:items-center sm:justify-between">
          <div>
            <p className="font-mono text-[10px] font-black tracking-[0.16em] text-[#2f6fce]">
              CONTINUE LEARNING
            </p>
            <p className="mt-1 text-sm font-bold">
              Turn the mental model into working knowledge.
            </p>
          </div>
          <Link
            href="/library"
            className="inline-flex min-h-11 w-fit items-center gap-2 rounded-lg bg-[#163052] px-4 py-2 text-sm font-bold text-white hover:bg-[#23466f] focus-visible:ring-2 focus-visible:ring-[#2f6fce] focus-visible:ring-offset-2 focus-visible:outline-none"
          >
            <BookOpen className="size-4" aria-hidden="true" />
            Open the Library
          </Link>
        </div>
      </article>
    </details>
  )
}

function CurrentSubjectDiagram() {
  return (
    <aside className="self-start overflow-hidden rounded-xl border-2 border-[#163052] bg-white shadow-[7px_7px_0_#dbe5f2]">
      <div className="flex items-center justify-between border-b-2 border-[#163052] px-4 py-3">
        <span className="font-mono text-[9px] font-black tracking-[0.17em]">
          CURRENT SUBJECT
        </span>
        <span className="size-2 rounded-full bg-[#3d9b72]" aria-hidden="true" />
      </div>
      <div className="p-4">
        <div className="rounded-md bg-[#163052] px-3 py-2 font-mono text-[10px] font-black text-white">
          ONE COMMIT
        </div>
        <div className="mx-auto h-5 w-px bg-[#163052]" aria-hidden="true" />
        <div className="grid grid-cols-2 gap-3">
          <DiagramLane
            icon={GitCommit}
            label="Git history"
            value="code + pointer"
            color="#2f6fce"
          />
          <DiagramLane
            icon={Cloud}
            label="Object store"
            value="large bytes"
            color="#e9784a"
          />
        </div>
        <p className="m-0 mt-3 border border-[#3d9b72] bg-[#e9f6ef] px-3 py-2 text-center text-xs font-bold text-[#287754]">
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
  color,
}: {
  icon: typeof GitCommit
  label: string
  value: string
  color: string
}) {
  return (
    <div
      className="border border-[#b9c7d8] p-3"
      style={{ borderTop: `4px solid ${color}` }}
    >
      <Icon className="size-4" style={{ color }} aria-hidden="true" />
      <p className="m-0 mt-3 text-[10px] font-black tracking-wide uppercase">
        {label}
      </p>
      <p className="m-0 mt-1 text-[10px] leading-4 text-[#607188]">{value}</p>
    </div>
  )
}
