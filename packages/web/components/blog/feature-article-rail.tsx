"use client"

import { List, MoveUpRight } from "lucide-react"
import { useEffect, useState } from "react"

import { cn } from "@/lib/utils"

export type FeatureArticleTocItem = {
  title: string
  url: string
  depth: number
}

function useReadingPosition(items: FeatureArticleTocItem[]) {
  const [activeUrl, setActiveUrl] = useState(items[0]?.url ?? "")
  const [progress, setProgress] = useState(0)

  useEffect(() => {
    let frame = 0

    const update = () => {
      frame = 0
      const article = document.querySelector<HTMLElement>(
        "[data-feature-article]"
      )
      if (!article) return

      const start = article.offsetTop
      const available = Math.max(1, article.offsetHeight - window.innerHeight)
      const nextProgress = Math.min(
        100,
        Math.max(0, ((window.scrollY - start) / available) * 100)
      )
      setProgress(Math.round(nextProgress))

      const headings = items
        .map((item) => document.getElementById(item.url.slice(1)))
        .filter((heading): heading is HTMLElement => heading !== null)
      const current = headings.reduce<HTMLElement | undefined>(
        (latest, heading) =>
          heading.getBoundingClientRect().top <= 180 ? heading : latest,
        undefined
      )

      if (current) setActiveUrl(`#${current.id}`)
    }

    const scheduleUpdate = () => {
      if (frame) return
      frame = window.requestAnimationFrame(update)
    }

    update()
    window.addEventListener("scroll", scheduleUpdate, { passive: true })
    window.addEventListener("resize", scheduleUpdate)

    return () => {
      if (frame) window.cancelAnimationFrame(frame)
      window.removeEventListener("scroll", scheduleUpdate)
      window.removeEventListener("resize", scheduleUpdate)
    }
  }, [items])

  return { activeUrl, progress }
}

export function FeatureArticleRail({
  items,
}: {
  items: FeatureArticleTocItem[]
}) {
  const { activeUrl, progress } = useReadingPosition(items)

  if (items.length === 0) return null

  return (
    <>
      <details className="group mb-10 rounded-xl border border-border bg-card p-4 lg:hidden">
        <summary className="flex min-h-8 cursor-pointer list-none items-center justify-between gap-4 text-sm font-semibold [&::-webkit-details-marker]:hidden">
          <span className="flex items-center gap-2">
            <List size={15} aria-hidden="true" />
            In this story
          </span>
          <span className="font-mono text-[10px] text-muted-foreground">
            {progress}% READ
          </span>
        </summary>
        <TocLinks items={items} activeUrl={activeUrl} className="mt-4" />
      </details>

      <nav
        aria-label="Article table of contents"
        className="sticky top-24 hidden max-h-[calc(100vh-8rem)] overflow-y-auto pr-4 lg:block"
      >
        <div className="flex items-center justify-between gap-3 font-mono text-[10px] font-semibold tracking-[0.16em] text-muted-foreground uppercase">
          <span>In this story</span>
          <span>{progress}%</span>
        </div>
        <div className="mt-3 h-px overflow-hidden bg-border">
          <div
            className="h-full bg-primary transition-[width] duration-150"
            style={{ width: `${progress}%` }}
          />
        </div>
        <TocLinks items={items} activeUrl={activeUrl} className="mt-4" />
      </nav>
    </>
  )
}

function TocLinks({
  items,
  activeUrl,
  className,
}: {
  items: FeatureArticleTocItem[]
  activeUrl: string
  className?: string
}) {
  return (
    <ol className={cn("space-y-1", className)}>
      {items.map((item) => {
        const active = item.url === activeUrl

        return (
          <li key={item.url}>
            <a
              href={item.url}
              aria-current={active ? "location" : undefined}
              className={cn(
                "group flex min-h-11 items-start gap-2 rounded-md border-l-2 py-2 pr-2 text-xs leading-5 transition-colors focus-visible:ring-2 focus-visible:ring-primary focus-visible:outline-none lg:min-h-9",
                item.depth > 2 ? "pl-5" : "pl-3",
                active
                  ? "border-primary bg-primary/5 font-medium text-foreground"
                  : "border-transparent text-muted-foreground hover:border-border hover:text-foreground"
              )}
            >
              <span className="min-w-0 flex-1">{item.title}</span>
              {active && (
                <MoveUpRight
                  size={12}
                  className="mt-1 shrink-0 text-primary"
                  aria-hidden="true"
                />
              )}
            </a>
          </li>
        )
      })}
    </ol>
  )
}
