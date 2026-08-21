import type { ReactNode } from "react"
import Link from "next/link"
import type { LucideIcon } from "lucide-react"
import { ArrowRight } from "lucide-react"

import { MarketingLayout } from "@/components/marketing-layout"
import { DiagramBox } from "@/components/marketing/diagram-box"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"

interface SummaryItem {
  icon: LucideIcon
  title: string
  description: string
}

interface SectionLink {
  id: string
  label: string
}

interface LegalPageProps {
  eyebrow: string
  title: string
  intro: string
  lastUpdated?: string
  summaryItems: SummaryItem[]
  sectionLinks: SectionLink[]
  diagram?: ReactNode
  children: ReactNode
}

export function LegalPage({
  eyebrow,
  title,
  intro,
  lastUpdated,
  summaryItems,
  sectionLinks,
  diagram,
  children,
}: LegalPageProps) {
  return (
    <MarketingLayout>
      <section className="border-b bg-muted/30">
        <div className="mx-auto grid max-w-7xl gap-10 px-6 pt-28 pb-14 md:grid-cols-[minmax(0,1fr)_18rem] md:pt-32 md:pb-16">
          <div>
            <Badge variant="outline">{eyebrow}</Badge>
            <h1 className="mt-6 max-w-4xl text-4xl font-bold tracking-tight text-foreground md:text-6xl">
              {title}
            </h1>
            <p className="mt-6 max-w-3xl text-lg leading-8 text-muted-foreground md:text-xl">
              {intro}
            </p>
            {lastUpdated && (
              <p className="mt-5 text-sm text-muted-foreground">
                Last updated: {lastUpdated}
              </p>
            )}
          </div>

          <aside className="h-fit rounded-lg border bg-background/80 p-5 shadow-sm md:sticky md:top-24">
            <p className="text-sm font-semibold text-foreground">
              On this page
            </p>
            <nav className="mt-4 flex flex-col gap-1 text-sm">
              {sectionLinks.map((link) => (
                <a
                  key={link.id}
                  href={`#${link.id}`}
                  className="rounded-md px-2 py-1.5 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/30 focus-visible:outline-none"
                >
                  {link.label}
                </a>
              ))}
            </nav>
          </aside>
        </div>
      </section>

      <section className="border-b">
        <div className="mx-auto grid max-w-7xl gap-5 px-6 py-10 md:grid-cols-3">
          {summaryItems.map((item) => (
            <div
              key={item.title}
              className="rounded-lg border bg-card p-5 shadow-sm transition-colors hover:border-primary/30"
            >
              <div className="flex h-10 w-10 items-center justify-center rounded-md bg-primary/10 text-primary">
                <item.icon className="h-5 w-5" aria-hidden="true" />
              </div>
              <h2 className="mt-4 text-base font-semibold text-foreground">
                {item.title}
              </h2>
              <p className="mt-2 text-sm leading-6 text-muted-foreground">
                {item.description}
              </p>
            </div>
          ))}
        </div>
      </section>

      {diagram && (
        <section className="mx-auto max-w-6xl px-6 py-16">
          <DiagramBox>{diagram}</DiagramBox>
        </section>
      )}

      <section className="mx-auto grid max-w-7xl gap-10 px-6 pt-16 pb-24 md:grid-cols-[minmax(0,1fr)_18rem]">
        <article className="max-w-3xl min-w-0">{children}</article>
        <aside className="h-fit rounded-lg border bg-card p-5 shadow-sm md:sticky md:top-24">
          <p className="text-sm font-semibold text-foreground">Helpful links</p>
          <p className="mt-2 text-sm leading-6 text-muted-foreground">
            Jump into the operational docs or reach the team.
          </p>
          <div className="mt-4 flex flex-col gap-3">
            <Button
              variant="outline"
              size="lg"
              className="w-full justify-between"
              render={<Link href="/docs/cli" />}
            >
              CLI docs
              <ArrowRight />
            </Button>
            <Button
              variant="outline"
              size="lg"
              render={
                <a
                  href="https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog"
                  target="_blank"
                  rel="noopener noreferrer"
                />
              }
              className="w-full justify-between"
            >
              Contact us
              <ArrowRight />
            </Button>
          </div>
        </aside>
      </section>
    </MarketingLayout>
  )
}

export function LegalSection({
  id,
  title,
  children,
}: {
  id: string
  title: string
  children: ReactNode
}) {
  return (
    <section
      id={id}
      className="scroll-mt-24 border-t py-12 first:border-t-0 first:pt-0"
    >
      <h2 className="text-2xl font-semibold tracking-tight text-foreground">
        {title}
      </h2>
      <div className="mt-5 space-y-4 text-base leading-7 text-muted-foreground [&_a]:font-medium [&_a]:text-primary [&_a]:underline-offset-4 [&_a:hover]:underline [&_li]:pl-1 [&_strong]:text-foreground">
        {children}
      </div>
    </section>
  )
}
