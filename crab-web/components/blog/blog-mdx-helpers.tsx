import { ArrowRight, CheckCircle2, GitBranch, Info } from "lucide-react"

import { cn } from "@/lib/utils"

export function TakeawayBox({
  title = "Key takeaway",
  children,
}: {
  title?: string
  children: React.ReactNode
}) {
  return (
    <aside className="my-8 rounded-lg border border-primary/20 bg-primary/5 p-5">
      <div className="flex items-center gap-2 text-sm font-semibold text-primary">
        <CheckCircle2 size={16} />
        {title}
      </div>
      <div className="mt-3 text-sm leading-7 text-foreground [&>p:last-child]:mb-0">
        {children}
      </div>
    </aside>
  )
}

export function ConceptChecklist({
  title = "Concepts in this guide",
  items,
}: {
  title?: string
  items: string[]
}) {
  return (
    <section className="my-8 rounded-lg border border-border bg-card p-5">
      <div className="text-sm font-semibold text-foreground">{title}</div>
      <ul className="mt-4 grid gap-2 pl-0 sm:grid-cols-2">
        {items.map((item) => (
          <li key={item} className="flex gap-2 text-sm text-muted-foreground">
            <CheckCircle2
              size={15}
              className="mt-0.5 shrink-0 text-primary"
              aria-hidden="true"
            />
            <span>{item}</span>
          </li>
        ))}
      </ul>
    </section>
  )
}

export function FlowSteps({
  title,
  steps,
}: {
  title: string
  steps: string[]
}) {
  return (
    <section className="my-8 rounded-lg border border-border bg-card p-5">
      <div className="flex items-center gap-2 text-sm font-semibold text-foreground">
        <GitBranch size={16} className="text-primary" />
        {title}
      </div>
      <ol className="mt-5 space-y-3 pl-0">
        {steps.map((step, index) => (
          <li key={step} className="flex gap-3">
            <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-primary/10 text-xs font-semibold text-primary">
              {index + 1}
            </span>
            <span className="pt-0.5 text-sm leading-6 text-muted-foreground">
              {step}
            </span>
          </li>
        ))}
      </ol>
    </section>
  )
}

export function BeforeAfter({
  before,
  after,
}: {
  before: string[]
  after: string[]
}) {
  return (
    <section className="my-8 grid gap-4 sm:grid-cols-2">
      <ComparisonColumn title="Before Crab" items={before} muted />
      <ComparisonColumn title="With Crab" items={after} />
    </section>
  )
}

function ComparisonColumn({
  title,
  items,
  muted = false,
}: {
  title: string
  items: string[]
  muted?: boolean
}) {
  return (
    <div
      className={cn(
        "rounded-lg border p-5",
        muted ? "border-border bg-muted/30" : "border-primary/20 bg-primary/5"
      )}
    >
      <div className="text-sm font-semibold text-foreground">{title}</div>
      <ul className="mt-4 space-y-2 pl-0">
        {items.map((item) => (
          <li key={item} className="flex gap-2 text-sm leading-6 text-muted-foreground">
            <ArrowRight
              size={14}
              className={cn("mt-1 shrink-0", muted ? "text-muted-foreground" : "text-primary")}
              aria-hidden="true"
            />
            {item}
          </li>
        ))}
      </ul>
    </div>
  )
}

export function SystemNote({
  title = "System note",
  children,
}: {
  title?: string
  children: React.ReactNode
}) {
  return (
    <aside className="my-8 rounded-lg border border-border bg-muted/30 p-5">
      <div className="flex items-center gap-2 text-sm font-semibold text-foreground">
        <Info size={16} className="text-primary" />
        {title}
      </div>
      <div className="mt-3 text-sm leading-7 text-muted-foreground [&>p:last-child]:mb-0">
        {children}
      </div>
    </aside>
  )
}
