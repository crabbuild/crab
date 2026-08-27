import {
  Cpu,
  Layers,
  Package,
  Zap,
  Cloud,
  Link2,
  CheckCircle,
  BookOpen,
  Scissors,
  Filter,
  Boxes,
  UploadCloud,
  GitMerge,
  MailIcon,
} from "lucide-react"

import { MarketingLayout } from "@/components/marketing-layout"
import { HeroSection } from "@/components/marketing/hero-section"
import { FeatureCard } from "@/components/marketing/feature-card"
import { DiagramBox } from "@/components/marketing/diagram-box"
import { CTASection } from "@/components/marketing/cta-section"
import { ComparisonTable } from "@/components/marketing/comparison-table"
import { Reveal } from "@/components/marketing/reveal"
import { TypingCode } from "@/components/marketing/typing-code"
import { Counter } from "@/components/marketing/counter"
import {
  InstallTabIcons,
  InstallTabs,
  type InstallTab,
} from "@/components/marketing/install-tabs"
import { CliPushPipelineSvg } from "@/app/diagrams/cli-push-pipeline-svg"
import { cn } from "@/lib/utils"

import { InteractiveCliSandbox } from "@/components/marketing/interactive-cli-sandbox"
import { createPageMetadata } from "@/lib/metadata"

export const metadata = createPageMetadata({
  title: "Crab CLI",
  description:
    "A single Rust binary that acts as a Git remote helper and filter driver for cloud object storage. Push and pull large files to S3, GCS, or Azure with standard Git commands.",
  path: "/cli",
})

const features = [
  {
    icon: Cpu,
    title: "Gearhash CDC",
    description:
      "SIMD-accelerated content-defined chunking at 500+ MB/s. Variable-size chunks ensure deduplication is resilient to insertions and deletions.",
  },
  {
    icon: Layers,
    title: "3-Tier Dedup",
    description:
      "Chunks are deduplicated at three levels — session, shard, and DB index — minimizing storage costs without sacrificing performance.",
  },
  {
    icon: Package,
    title: "Xorb Compressed Storage",
    description:
      "Deduplicated chunks are packed into compressed xorb objects for efficient storage and transfer in ~64 MiB batches.",
  },
  {
    icon: Zap,
    title: "Lazy Checkout",
    description:
      "Only materialize the files you need. Large repositories can be cloned instantly and files hydrated on demand.",
  },
  {
    icon: CheckCircle,
    title: "Fail-Forward Recovery",
    description:
      "Resumable uploads with checkpoint journaling. Interrupted pushes pick up where they left off — no re-uploading completed xorbs.",
  },
  {
    icon: Link2,
    title: "Git LFS Integration",
    description:
      "LFS-tracked files stored alongside xorbs through Crab's repository-scoped standalone transfer agent.",
  },
]

const providers = [
  { icon: Cloud, name: "AWS S3" },
  { icon: Cloud, name: "Google Cloud Storage" },
  { icon: Cloud, name: "Azure Blob Storage" },
]

/**
 * Per-platform installation snippets rendered inside the `InstallTabs`
 * component. Each entry is a self-contained `<pre><code>` block with a
 * mix of command/output/comment lines styled like a terminal session.
 *
 * Lines are kept short (~70 chars) so they don't horizontally overflow on
 * mobile inside the dark snippet frame.
 */
const installTabs: InstallTab[] = [
  {
    value: "macos",
    label: "macOS",
    icon: InstallTabIcons.macOS,
    lines: [
      { text: "# Install via Homebrew", type: "comment" },
      { text: "brew install crabbuild/tap/crab", type: "command" },
      { text: "" },
      { text: "# Confirm the install", type: "comment" },
      { text: "crab --version", type: "command" },
      { text: "crab 1.0.15", type: "output" },
    ],
    note: (
      <>
        Apple Silicon and Intel are both supported. Homebrew installs the{" "}
        <code className="font-mono text-foreground">crab</code> binary and
        the <code className="font-mono text-foreground">git-remote-crab</code>{" "}
        symlink in one step.
      </>
    ),
  },
  {
    value: "linux",
    label: "Linux",
    icon: InstallTabIcons.Linux,
    lines: [
      { text: "# One-line install (x86_64 and arm64)", type: "comment" },
      { text: "curl -fsSL https://crab.build/install.sh | bash", type: "command" },
      { text: "" },
      { text: "# Verify the binary is on PATH", type: "comment" },
      { text: "crab --version", type: "command" },
      { text: "crab 1.0.15", type: "output" },
    ],
    note: (
      <>
        The installer drops the binary in{" "}
        <code className="font-mono text-foreground">~/.crab/bin</code> and
        creates the{" "}
        <code className="font-mono text-foreground">git-remote-crab</code>{" "}
        symlink. Inspect the script before piping to{" "}
        <code className="font-mono text-foreground">bash</code> if you prefer.
      </>
    ),
  },
  {
    value: "windows",
    label: "Windows / WSL",
    icon: InstallTabIcons.Windows,
    lines: [
      { text: "# Native Windows in PowerShell", type: "comment" },
      { text: "irm https://crab.build/install.ps1 | iex", type: "command" },
      { text: "" },
      { text: "# Or inside WSL Ubuntu — same one-liner as Linux", type: "comment" },
      { text: "curl -fsSL https://crab.build/install.sh | bash", type: "command" },
      { text: "crab --version", type: "command" },
      { text: "crab 1.0.15", type: "output" },
    ],
    note: (
      <>
        Native Windows installs <code className="font-mono text-foreground">crab.exe</code>{" "}
        and <code className="font-mono text-foreground">git-remote-crab.exe</code>.
        Use WSL2 when you need Linux-only FUSE mounts.
      </>
    ),
  },
]

/**
 * Lines for the hero terminal preview. Showcases a `crab clone crab://...`
 * invocation with representative output. The full block stays under ten
 * lines so the typing animation completes inside the visible viewport
 * without scrolling.
 */
const heroCloneLines = [
  { text: "crab clone crab://bucket/repo my-repo", type: "command" as const },
  { text: "Cloning into 'my-repo'...", type: "output" as const },
  {
    text: "remote: resolving refs @ s3://bucket/repo/refs/heads/main",
    type: "output" as const,
  },
  {
    text: "Receiving objects: 100% (4821/4821), 18.4 MiB | 22.1 MiB/s, done.",
    type: "output" as const,
  },
  {
    text: "Configuring lazy checkout — files checked out as pointers.",
    type: "output" as const,
  },
  { text: "✔ ready — run `crab hydrate` to materialize files", type: "comment" as const },
]

/**
 * The five-stage push pipeline surfaced in the "How It Works" section.
 * Each `description` is constrained to 20–200 characters so the inline
 * tooltip stays compact while still being informative (Requirement 5.3).
 */
interface PipelineStep {
  id: string
  icon: typeof Cpu
  label: string
  shortName: string
  description: string
}

const pipelineSteps: PipelineStep[] = [
  {
    id: "cdc-chunking",
    icon: Scissors,
    label: "CDC Chunking",
    shortName: "Chunk",
    description:
      "Gearhash content-defined chunking splits files into variable-size chunks at SIMD speed (~500 MB/s), resilient to insertions and deletions.",
  },
  {
    id: "dedup-classification",
    icon: Filter,
    label: "Dedup Classification",
    shortName: "Dedup",
    description:
      "Each chunk is classified A (already in DB), B (in shard), or C (new). Only C-class chunks travel to the cloud — saving bandwidth and storage.",
  },
  {
    id: "xorb-packing",
    icon: Boxes,
    label: "Xorb Packing",
    shortName: "Pack",
    description:
      "New chunks are concatenated and compressed into ~64 MiB xorb objects so a few large PUTs replace thousands of tiny ones.",
  },
  {
    id: "parallel-upload",
    icon: UploadCloud,
    label: "Parallel Upload",
    shortName: "Upload",
    description:
      "Xorbs and shards stream to S3, GCS, or Azure over a parallel transfer pool with checkpoint journaling — interrupted pushes resume cleanly.",
  },
  {
    id: "ref-update",
    icon: GitMerge,
    label: "Ref Update",
    shortName: "Ref",
    description:
      "After all data lands, the ref is advanced via a compare-and-swap on the manifest object so concurrent pushes never race past each other.",
  },
]

/**
 * Renders the five-stage push pipeline as a row of focusable step buttons
 * connected by directional arrows, with a description panel that surfaces
 * on hover/focus (Requirement 5.2 / 5.3).
 *
 * Implemented purely with CSS state selectors so the section remains a
 * Server Component. Each step uses `<button type="button">` to be reachable
 * via Tab; the visual highlight and tooltip are driven by `:hover`,
 * `:focus-visible`, and `group-focus-within`.
 */
function PipelineStepsGrid({ steps }: { steps: PipelineStep[] }) {
  return (
    <div>
      <ol
        className="grid grid-cols-1 gap-4 sm:grid-cols-2 md:grid-cols-5"
        role="list"
      >
        {steps.map((step, index) => (
          <li key={step.id} className="relative">
            <PipelineStepCard step={step} index={index} />
            {/* Directional connector to the next step — visible only on
                medium+ viewports where the steps lay out horizontally. */}
            {index < steps.length - 1 && (
              <span
                aria-hidden="true"
                className="pointer-events-none absolute -right-3 top-9 hidden h-px w-6 bg-primary/40 md:block"
              >
                <span className="absolute -right-px top-[-3px] h-0 w-0 border-y-[3px] border-l-[6px] border-y-transparent border-l-primary/60" />
              </span>
            )}
          </li>
        ))}
      </ol>
      <p className="mt-6 text-center text-sm text-muted-foreground">
        Tab through the steps with your keyboard for the same details.
      </p>
    </div>
  )
}

function PipelineStepCard({
  step,
  index,
}: {
  step: PipelineStep
  index: number
}) {
  const Icon = step.icon
  const tooltipId = `pipeline-step-${step.id}-tooltip`
  return (
    <div className="group relative h-full">
      <button
        type="button"
        aria-describedby={tooltipId}
        className={cn(
          "relative flex h-full w-full flex-col items-start gap-3 rounded-card border border-border bg-card p-5 text-left",
          "shadow-card transition-[background-color,border-color,box-shadow,transform] duration-(--duration-normal) ease-(--ease-out-app)",
          "hover:-translate-y-0.5 hover:border-primary/60 hover:bg-primary-muted/40 hover:shadow-card-hover",
          "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background",
          "focus-visible:border-primary focus-visible:bg-primary-muted/40",
        )}
      >
        <div className="flex w-full items-center justify-between">
          <span className="inline-flex h-9 w-9 items-center justify-center rounded-md bg-primary-muted text-primary transition-colors duration-(--duration-fast) group-hover:bg-primary group-hover:text-primary-foreground group-focus-within:bg-primary group-focus-within:text-primary-foreground">
            <Icon size={18} strokeWidth={2} aria-hidden="true" />
          </span>
          <span className="font-mono text-[11px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
            Step {index + 1}
          </span>
        </div>
        <h3 className="text-base font-semibold text-foreground">
          {step.label}
        </h3>
        <span className="text-xs font-medium text-muted-foreground">
          {step.shortName}
        </span>
      </button>

      {/*
       * Tooltip / expanded description.
       *
       * Hidden by default; revealed via `group-hover` and `group-focus-within`
       * so the same affordance fires for both pointer and keyboard users.
       * `role="tooltip"` + `aria-describedby` on the button ties it to
       * assistive tech, and the tooltip is rendered in the DOM so screen
       * readers announce the description on focus.
       */}
      <div
        id={tooltipId}
        role="tooltip"
        className={cn(
          "pointer-events-none absolute left-1/2 top-full z-10 mt-2 w-64 -translate-x-1/2 rounded-card border border-border bg-popover p-3 text-xs text-popover-foreground shadow-card-hover",
          "opacity-0 translate-y-1 transition-[opacity,transform] duration-(--duration-fast) ease-(--ease-out-app)",
          "group-hover:opacity-100 group-hover:translate-y-0 group-focus-within:opacity-100 group-focus-within:translate-y-0",
        )}
      >
        {step.description}
      </div>
    </div>
  )
}

/**
 * Performance benchmarks displayed as animated counters in the
 * "Performance Benchmarks" section. Numbers are taken from the shipping
 * CLI's chunking + push pipeline (Requirement 5.4).
 *
 * Each entry has an integer `end` (Counter only animates integers), a
 * `suffix` rendered alongside the value, a short `label`, and a one-line
 * `caption` that explains where the metric comes from.
 */
const benchmarkMetrics: Array<{
  end: number
  suffix: string
  label: string
  caption: string
}> = [
  {
    end: 500,
    suffix: "+ MB/s",
    label: "Chunking throughput",
    caption: "Gearhash CDC on a single core, SIMD-accelerated.",
  },
  {
    end: 16,
    suffix: "×",
    label: "Upload parallelism",
    caption: "Xorbs stream concurrently to the object store.",
  },
  {
    end: 70,
    suffix: "%",
    label: "Typical dedup ratio",
    caption: "Bytes saved on incremental pushes of large repos.",
  },
  {
    end: 64,
    suffix: " MiB",
    label: "Xorb pack size",
    caption: "Many tiny PUTs collapsed into a few large ones.",
  },
]

/**
 * Comparison matrix for the "Crab vs. alternatives" section. Compares the
 * Crab CLI against Git LFS and DVC across nine feature dimensions covering
 * the eight required by the spec (server requirement, dedup method, max
 * file size, lazy checkout, resumable uploads, cloud provider support,
 * Git compatibility, open-source license) plus a partial-transfer row.
 *
 * String values are kept short so the row remains readable on the
 * mobile horizontal-scroll layout used by `ComparisonTable`.
 */
const comparisonData: {
  headers: string[]
  rows: Array<{ label: string; values: Array<boolean | string> }>
} = {
  headers: ["Crab", "Git LFS", "DVC"],
  rows: [
    {
      label: "No server required",
      values: [true, false, true],
    },
    {
      label: "Deduplication method",
      values: [
        "Content-defined chunking (gearhash)",
        "None (whole-file)",
        "Whole-file content hashing",
      ],
    },
    {
      label: "Maximum file size",
      values: ["TB-scale (chunked)", "5 GB (GitHub), server-dependent", "Bucket-limited"],
    },
    {
      label: "Lazy checkout / partial materialization",
      values: [true, "Include/exclude patterns only", false],
    },
    {
      label: "Resumable uploads",
      values: [true, false, false],
    },
    {
      label: "Cloud provider support",
      values: ["S3, GCS, Azure", "Self-hosted or SaaS LFS server", "S3, GCS, Azure, SSH, HDFS, local"],
    },
    {
      label: "Standard Git UX",
      values: [true, true, "Parallel CLI (dvc push/pull)"],
    },
    {
      label: "Chunk-level delta transfer",
      values: [true, false, false],
    },
  ],
}

export default function CliPage() {
  return (
    <MarketingLayout>
      {/* Hero */}
      <HeroSection
        badge={{ text: "Serverless Git Remote", dot: true }}
        headline={
          <>
            One binary.{" "}
            <span className="text-primary">Zero servers.</span>
            <br />
            Git for cloud object storage.
          </>
        }
        subheadline="A single Rust binary that acts as both a Git remote helper and a filter driver. Push and pull repositories backed by S3, GCS, or Azure with standard Git commands — no servers, no LFS endpoints, no infrastructure to manage."
        primaryCTA={{
          label: "Read the Docs",
          href: "/docs/cli",
          icon: BookOpen,
        }}
        secondaryCTA={{
          label: "Contact Us",
          href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog",
          icon: MailIcon,
        }}
        animatedBackground="particles"
        headlineEffect="shimmer"
        diagram={
          <div className="mx-auto max-w-3xl">
            <TypingCode
              title="Crab CLI"
              lines={heroCloneLines}
              charDelay={28}
              charJitter={18}
              lineDelay={550}
              threshold={0.4}
            />
          </div>
        }
      />

      {/* Interactive Command Sandbox */}
      <section className="bg-background border-b border-border py-16 md:py-24">
        <div className="mx-auto max-w-6xl px-6">
          <Reveal>
            <InteractiveCliSandbox />
          </Reveal>
        </div>
      </section>

      {/* How It Works — 5-step push pipeline with hover/focus tooltips */}
      <section
        className="mx-auto max-w-6xl px-6 py-16 md:py-24"
        aria-labelledby="how-it-works-heading"
      >
        <Reveal>
          <div className="mb-3 text-center">
            <span className="font-mono text-xs uppercase tracking-[0.18em] text-primary">
              How It Works
            </span>
          </div>
          <div className="text-center mb-12">
            <h2
              id="how-it-works-heading"
              className="text-3xl font-bold tracking-tight text-foreground"
            >
              Five Stages, From Working Tree to Cloud
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              Hover or focus a step to see what happens under the hood.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <PipelineStepsGrid steps={pipelineSteps} />
        </Reveal>
      </section>

      {/* Performance Benchmarks — animated counters (Requirement 5.4) */}
      <section
        className="mx-auto max-w-6xl px-6 py-16 md:py-24"
        aria-labelledby="benchmarks-heading"
      >
        <Reveal>
          <div className="mb-3 text-center">
            <span className="font-mono text-xs uppercase tracking-[0.18em] text-primary">
              Performance Benchmarks
            </span>
          </div>
          <div className="text-center mb-12">
            <h2
              id="benchmarks-heading"
              className="text-3xl font-bold tracking-tight text-foreground"
            >
              Built for Scale, Tuned for Speed
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              Numbers from the shipping CLI on commodity hardware.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <ul
            role="list"
            className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4 lg:gap-6"
          >
            {benchmarkMetrics.map((metric) => (
              <li key={metric.label}>
                <div className="h-full rounded-card border border-border bg-card p-6 text-center shadow-card transition-shadow duration-(--duration-normal) ease-(--ease-out-app) hover:shadow-card-hover">
                  <div className="font-heading text-3xl font-extrabold tracking-tight text-foreground md:text-4xl">
                    <Counter end={metric.end} suffix={metric.suffix} />
                  </div>
                  <div className="mt-2 text-sm font-semibold text-foreground">
                    {metric.label}
                  </div>
                  <p className="mt-2 text-xs leading-relaxed text-muted-foreground">
                    {metric.caption}
                  </p>
                </div>
              </li>
            ))}
          </ul>
        </Reveal>
      </section>

      {/* Feature Cards */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              Technical Differentiators
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              Purpose-built for large-file workflows at scale.
            </p>
          </div>
        </Reveal>
          <div className="grid grid-cols-1 gap-6 sm:grid-cols-2 lg:grid-cols-3">
            {features.map((feature) => (
              <FeatureCard
                key={feature.title}
                icon={feature.icon}
                title={feature.title}
                description={feature.description}
                className="glass h-full"
              />
            ))}
          </div>
      </section>

      {/* Push Pipeline Diagram */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              Push Pipeline
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              From local files to cloud storage in five stages.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <DiagramBox>
            <CliPushPipelineSvg />
          </DiagramBox>
        </Reveal>
      </section>

      {/* Crab vs. alternatives — feature comparison matrix */}
      <section
        className="mx-auto max-w-6xl px-6 py-16 md:py-24"
        aria-labelledby="comparison-heading"
      >
        <Reveal>
          <div className="mb-3 text-center">
            <span className="font-mono text-xs uppercase tracking-[0.18em] text-primary">
              Crab vs. Alternatives
            </span>
          </div>
          <div className="text-center mb-12">
            <h2
              id="comparison-heading"
              className="text-3xl font-bold tracking-tight text-foreground"
            >
              How Crab Compares to Git LFS and DVC
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              Eight feature dimensions, side by side.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <ComparisonTable
            headers={comparisonData.headers}
            rows={comparisonData.rows}
          />
        </Reveal>
      </section>

      {/* Cloud Providers */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              Supported Cloud Providers
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              Works with the major cloud object storage providers out of the box.
            </p>
          </div>
        </Reveal>
          <div className="grid grid-cols-1 gap-6 sm:grid-cols-3">
            {providers.map((provider) => (
              <div
                key={provider.name}
                className="flex items-center gap-4 rounded-xl border border-border bg-card/60 p-6 glass glow-on-hover"
              >
                <div className="inline-flex h-10 w-10 items-center justify-center rounded-md bg-primary-muted text-primary">
                  <provider.icon size={20} strokeWidth={2} />
                </div>
                <span className="text-sm font-semibold text-foreground">
                  {provider.name}
                </span>
              </div>
            ))}
          </div>
      </section>

      {/* Installation — tabbed code blocks per platform */}
      <section
        className="mx-auto max-w-6xl px-6 py-16 md:py-24"
        aria-labelledby="installation-heading"
      >
        <Reveal>
          <div className="mb-3 text-center">
            <span className="font-mono text-xs uppercase tracking-[0.18em] text-primary">
              Installation
            </span>
          </div>
          <div className="text-center mb-12">
            <h2
              id="installation-heading"
              className="text-3xl font-bold tracking-tight text-foreground"
            >
              Install Crab on Your Platform
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              One binary, copy-paste install. Pick your platform below.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <div className="mx-auto max-w-3xl">
            <InstallTabs tabs={installTabs} />
          </div>
        </Reveal>
      </section>

      {/* CTA */}
      <Reveal>
        <CTASection
          headline="Ready to ditch your LFS server?"
          description="Start pushing large files to cloud storage with standard Git commands. No infrastructure required."
          primaryCTA={{
            label: "Read the Documentation",
            href: "/docs/cli",
            icon: BookOpen,
          }}
          secondaryCTA={{
            label: "Contact Us",
            href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog",
            icon: MailIcon,
          }}
        />
      </Reveal>
    </MarketingLayout>
  )
}
