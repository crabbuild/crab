import type { LucideIcon } from "lucide-react"
import { Cloud, Download, GitBranch } from "lucide-react"

import { Reveal } from "@/components/marketing/reveal"

/* ------------------------------------------------------------------ */
/*  Types                                                              */
/* ------------------------------------------------------------------ */

interface Step {
  /** Two-digit step number label, e.g. "01" */
  number: string
  icon: LucideIcon
  title: string
  description: string
  /** Shell / terminal snippet shown in the mini-terminal block */
  code: string
}

/* ------------------------------------------------------------------ */
/*  Data                                                               */
/* ------------------------------------------------------------------ */

const steps: Step[] = [
  {
    number: "01",
    icon: Download,
    title: "Install & Init",
    description:
      "Install Crab, connect your bucket, then scan for large files and configure git tracking.",
    code: "crab init --storage-provider s3 crab://my-bucket/repo && crab setup",
  },
  {
    number: "02",
    icon: GitBranch,
    title: "Ship Your Files",
    description:
      "A single command stages, commits, and pushes. Crab handles chunking, deduplication, and parallel upload.",
    code: 'crab ship . -m "add model v2"',
  },
  {
    number: "03",
    icon: Cloud,
    title: "Stored in Your Cloud",
    description:
      "Files are deduplicated and packed into xorbs in your own S3, GCS, or Azure bucket. No server needed.",
    code: "crab://my-bucket/repo ✓",
  },
]

/* ------------------------------------------------------------------ */
/*  Sub-components                                                     */
/* ------------------------------------------------------------------ */

/** Horizontal connector arrow rendered between cards on wide screens. */
function HorizontalConnector() {
  return (
    <div
      aria-hidden="true"
      className="hidden items-center md:flex"
    >
      <div className="h-px w-full border-t-2 border-dashed border-border" />
      {/* Arrowhead */}
      <div className="ml-[-6px] h-0 w-0 shrink-0 border-y-[5px] border-l-[7px] border-y-transparent border-l-primary" />
    </div>
  )
}

/** Vertical connector arrow rendered between cards on mobile. */
function VerticalConnector() {
  return (
    <div
      aria-hidden="true"
      className="flex flex-col items-center py-2 md:hidden"
    >
      <div className="w-px grow border-l-2 border-dashed border-border min-h-8" />
      {/* Arrowhead */}
      <div className="mt-[-6px] h-0 w-0 shrink-0 border-x-[5px] border-t-[7px] border-x-transparent border-t-primary" />
    </div>
  )
}

/** A single step card with number badge, icon, text, and code block. */
function StepCard({ step, index }: { step: Step; index: number }) {
  const Icon = step.icon

  return (
    <Reveal duration={450} threshold={0.1} className="flex-1">
      <article
        className={
          "relative flex h-full flex-col rounded-card border border-border bg-card p-card " +
          "shadow-card transition-shadow duration-(--duration-normal) ease-(--ease-out-app) hover:shadow-card-hover"
        }
      >
        {/* Title & description */}
        <h3 className="mb-4 font-heading text-heading-sm font-semibold text-foreground flex items-center gap-2">
          {/* Icon */}
          <div
            aria-hidden="true"
            className="inline-flex h-11 w-11 items-center justify-center rounded-lg bg-primary-muted text-primary"
          >
            <Icon size={22} strokeWidth={2} />
          </div>

          {/* Step number badge */}
          <span
            aria-label={`Step ${index + 1}`}
            className="inline-flex h-8 w-8 items-center justify-center rounded-full bg-primary text-xs font-bold text-primary-foreground"
          >
            {step.number}
          </span> <span>{step.title}</span>
        </h3>
        <p className="mb-4 text-sm leading-relaxed text-muted-foreground">
          {step.description}
        </p>

        {/* Mini-terminal code block */}
        <div className="mt-auto overflow-hidden rounded-lg bg-foreground">
          <div className="flex items-center gap-1.5 px-3 pt-2">
            <span className="h-2.5 w-2.5 rounded-full bg-red-400/70" />
            <span className="h-2.5 w-2.5 rounded-full bg-yellow-400/70" />
            <span className="h-2.5 w-2.5 rounded-full bg-green-400/70" />
          </div>
          <pre className="px-3 pb-3 pt-2">
            <code className="text-xs leading-relaxed text-background">
              <span className="select-none text-muted-foreground/60">$ </span>
              {step.code}
            </code>
          </pre>
        </div>
      </article>
    </Reveal>
  )
}

/* ------------------------------------------------------------------ */
/*  Main component                                                     */
/* ------------------------------------------------------------------ */

/**
 * "How It Works" section — three-step visual flow showing the Crab
 * install → push → stored workflow. Renders horizontally on wide layouts
 * (with dashed arrow connectors) and vertically on mobile (with
 * downward arrows).
 */
export function HowItWorks() {
  return (
    <section
      aria-labelledby="how-it-works-heading"
      className="mx-auto w-full max-w-6xl px-4 py-section sm:px-6 lg:px-8"
    >
      {/* Section header */}
      <Reveal className="mb-12 text-center">
        <h2
          id="how-it-works-heading"
          className="font-heading text-heading-xl font-bold tracking-tight text-foreground"
        >
          How It Works
        </h2>
        <p className="mx-auto mt-3 max-w-2xl text-lg text-muted-foreground">
          Three steps to version and store your large files — no servers, no
          proprietary lock-in.
        </p>
      </Reveal>

      {/* Step cards with connectors */}
      <div className="flex flex-col md:flex-row md:items-stretch gap-1 md:gap-1">
        {steps.map((step, index) => (
          <div
            key={step.number}
            className="flex flex-col md:flex-row md:items-stretch md:flex-1"
          >
            <StepCard step={step} index={index} />

            {/* Connector between cards (skip after last step) */}
            {index < steps.length - 1 && (
              <>
                <HorizontalConnector />
                <VerticalConnector />
              </>
            )}
          </div>
        ))}
      </div>
    </section>
  )
}
