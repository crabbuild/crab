import { ArrowLeft, ArrowRight, Clock, GitBranch, Sparkles } from "lucide-react"
import Link from "next/link"
import type { ReactNode } from "react"

import {
  FeatureArticleRail,
  type FeatureArticleTocItem,
} from "@/components/blog/feature-article-rail"
import { LargeFileScaleExplorer } from "@/components/blog/large-file-scale-explorer"
import { MarketingLayout } from "@/components/marketing-layout"
import { formatBlogDate } from "@/lib/blog-date"
import type { BlogPostMeta } from "@/lib/blog-posts"

export function FeatureBlogArticle({
  post,
  toc,
  children,
}: {
  post: BlogPostMeta
  toc: FeatureArticleTocItem[]
  children: ReactNode
}) {
  return (
    <MarketingLayout>
      <article className="overflow-clip border-b border-border bg-background">
        <header className="relative overflow-hidden bg-[#07111d] text-slate-100">
          <div
            className="pointer-events-none absolute inset-0 opacity-60"
            aria-hidden="true"
            style={{
              backgroundImage:
                "linear-gradient(rgba(148,163,184,.055) 1px, transparent 1px), linear-gradient(90deg, rgba(148,163,184,.055) 1px, transparent 1px)",
              backgroundSize: "44px 44px",
              maskImage:
                "linear-gradient(to bottom, black 0%, rgba(0,0,0,.65) 58%, transparent 100%)",
            }}
          />
          <div
            className="pointer-events-none absolute top-[-18rem] left-1/2 h-[42rem] w-[68rem] -translate-x-1/2 rounded-full bg-cyan-400/[0.08] blur-3xl"
            aria-hidden="true"
          />

          <div className="relative mx-auto max-w-7xl px-4 pt-24 pb-16 sm:px-6 sm:pt-28 lg:px-8 lg:pt-32 lg:pb-20">
            <Link
              href="/blog"
              className="inline-flex min-h-10 items-center gap-2 rounded-full border border-white/10 bg-white/[0.035] px-4 text-xs text-slate-300 transition-colors hover:border-white/20 hover:bg-white/[0.07] hover:text-white focus-visible:ring-2 focus-visible:ring-cyan-300 focus-visible:outline-none"
            >
              <ArrowLeft size={13} aria-hidden="true" />
              Crab blog
            </Link>

            <div className="mt-14 max-w-5xl">
              <div className="flex flex-wrap items-center gap-x-4 gap-y-2 font-mono text-[10px] font-semibold tracking-[0.18em] text-cyan-300 uppercase">
                <span className="inline-flex items-center gap-2">
                  <Sparkles size={13} aria-hidden="true" />
                  Launch story
                </span>
                <span className="h-px w-7 bg-cyan-300/35" aria-hidden="true" />
                <span>{post.category}</span>
              </div>

              <h1 className="mt-7 max-w-[14ch] text-[clamp(3.25rem,8vw,7.75rem)] leading-[0.88] font-semibold tracking-[-0.065em] text-balance text-white">
                {post.title}
              </h1>
              <p className="mt-8 max-w-3xl text-lg leading-8 text-slate-300 sm:text-xl sm:leading-9">
                {post.description}
              </p>

              <div className="mt-9 flex flex-wrap items-center gap-x-5 gap-y-3 text-sm text-slate-400">
                <span>{post.author}</span>
                <span
                  className="h-1 w-1 rounded-full bg-slate-600"
                  aria-hidden="true"
                />
                <time dateTime={post.date}>{formatBlogDate(post.date)}</time>
                <span
                  className="h-1 w-1 rounded-full bg-slate-600"
                  aria-hidden="true"
                />
                <span className="inline-flex items-center gap-1.5">
                  <Clock size={13} aria-hidden="true" />
                  {post.readingTimeMinutes} min read
                </span>
              </div>
            </div>

            <LargeFileScaleExplorer />
          </div>
        </header>

        <div
          data-feature-article
          className="mx-auto grid w-full max-w-[100rem] gap-10 px-4 py-16 sm:px-6 lg:grid-cols-[13rem_minmax(0,48rem)] lg:items-start lg:justify-center lg:gap-10 lg:px-8 lg:py-24 min-[90rem]:grid-cols-[14rem_minmax(0,62rem)_14rem] min-[90rem]:justify-between min-[90rem]:gap-8 2xl:gap-10 2xl:px-12"
        >
          <aside className="min-w-0 lg:sticky lg:top-24 lg:self-start">
            <FeatureArticleRail items={toc} />
          </aside>

          <div className="min-w-0">
            <div className="feature-blog-prose prose-lg prose-neutral dark:prose-invert prose max-w-none prose-headings:scroll-mt-24 prose-img:rounded-xl">
              {children}
            </div>
          </div>

          <aside className="hidden min-w-0 justify-self-end min-[90rem]:sticky min-[90rem]:top-24 min-[90rem]:right-0 min-[90rem]:col-start-3 min-[90rem]:row-start-1 min-[90rem]:block min-[90rem]:self-start">
            <div className="w-56 space-y-5">
              <div className="border-t-2 border-primary pt-4">
                <div className="flex items-center gap-2 font-mono text-[10px] font-semibold tracking-[0.16em] text-primary uppercase">
                  <GitBranch size={13} aria-hidden="true" />
                  The premise
                </div>
                <p className="mt-3 text-sm leading-6 text-muted-foreground">
                  Git should keep owning history. Your object store should keep
                  owning the heavy bytes. Crab makes the boundary explicit.
                </p>
                <svg
                  viewBox="0 0 224 104"
                  className="mt-5 h-auto w-full"
                  role="img"
                  aria-label="One commit points to Git history and a Crab pointer whose bytes live in object storage"
                >
                  <path
                    d="M46 52H92M92 52V25M92 52v27M92 25h29M92 79h29"
                    fill="none"
                    stroke="currentColor"
                    strokeWidth="1.5"
                    className="text-border"
                  />
                  <rect
                    x="4"
                    y="35"
                    width="42"
                    height="34"
                    rx="8"
                    fill="color-mix(in oklch, var(--primary) 9%, var(--card))"
                    stroke="var(--primary)"
                  />
                  <text
                    x="25"
                    y="55"
                    textAnchor="middle"
                    fill="var(--foreground)"
                    fontSize="8"
                    fontFamily="ui-monospace, monospace"
                  >
                    COMMIT
                  </text>
                  <rect
                    x="121"
                    y="9"
                    width="98"
                    height="33"
                    rx="8"
                    fill="var(--card)"
                    stroke="var(--border)"
                  />
                  <text
                    x="170"
                    y="29"
                    textAnchor="middle"
                    fill="var(--foreground)"
                    fontSize="8"
                    fontFamily="ui-monospace, monospace"
                  >
                    GIT HISTORY
                  </text>
                  <rect
                    x="121"
                    y="62"
                    width="98"
                    height="33"
                    rx="8"
                    fill="color-mix(in oklch, var(--primary) 7%, var(--card))"
                    stroke="var(--primary)"
                  />
                  <text
                    x="170"
                    y="82"
                    textAnchor="middle"
                    fill="var(--foreground)"
                    fontSize="8"
                    fontFamily="ui-monospace, monospace"
                  >
                    OBJECT STORE
                  </text>
                </svg>
              </div>

              <Link
                href="/docs/cli/getting-started/installation"
                className="group block rounded-xl border border-border bg-card p-4 transition-all hover:border-primary/30 hover:shadow-card-hover focus-visible:ring-2 focus-visible:ring-primary focus-visible:outline-none"
              >
                <span className="font-mono text-[10px] font-semibold tracking-[0.14em] text-muted-foreground uppercase">
                  Try Crab
                </span>
                <span className="mt-2 flex items-center justify-between gap-3 text-sm font-semibold text-foreground">
                  Install the CLI
                  <ArrowRight
                    size={14}
                    className="transition-transform group-hover:translate-x-0.5"
                    aria-hidden="true"
                  />
                </span>
              </Link>
            </div>
          </aside>
        </div>
      </article>
    </MarketingLayout>
  )
}
