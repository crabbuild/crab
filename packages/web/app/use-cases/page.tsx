import type { LucideIcon } from "lucide-react"
import {
  ArrowRight,
  BookOpen,
  Box,
  BrainCircuit,
  Check,
  Cloud,
  Database,
  Film,
  GitBranch,
  HardDrive,
  Layers3,
  MailIcon,
  RefreshCcw,
  ShieldCheck,
  Terminal,
  Workflow,
} from "lucide-react"
import Link from "next/link"

import {
  CiProfileVisual,
  CloudBoundaryVisual,
  CreativeWorkspaceVisual,
  DataSnapshotVisual,
  ModelLineageVisual,
  UseCasesOverviewVisual,
} from "@/app/diagrams/use-case-visuals"
import { MarketingLayout } from "@/components/marketing-layout"
import { CTASection } from "@/components/marketing/cta-section"
import { Reveal } from "@/components/marketing/reveal"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { cn } from "@/lib/utils"
import { createPageMetadata } from "@/lib/metadata"

export const metadata = createPageMetadata({
  title: "Use Cases",
  description:
    "See how ML, data, creative, CI, and platform teams version large files with Git while Crab stores deduplicated content in their object storage.",
  path: "/use-cases",
})

interface UseCase {
  id: string
  navLabel: string
  eyebrow: string
  icon: LucideIcon
  title: string
  description: string
  challenge: string
  workflow: string
  outcomes: string[]
  command: string
  commandLabel: string
  docsHref: string
  docsLabel: string
  visual: React.ReactNode
}

const useCases: UseCase[] = [
  {
    id: "ml-ai",
    navLabel: "ML & AI",
    eyebrow: "Models and experiments",
    icon: BrainCircuit,
    title: "Keep the checkpoint with the code that produced it.",
    description:
      "Put weights, adapters, training data, configs, and metrics on one Git timeline without turning large payloads into ordinary Git blobs.",
    challenge:
      "Model artifacts often live under bucket names or registry tags that drift away from the training commit. Repeated checkpoints can also contain long byte ranges that earlier versions already uploaded.",
    workflow:
      "Crab commits small pointer blobs to Git and stages the original bytes as content-defined chunks. On push, content-addressed deduplication can reuse chunks already present. A teammate can clone the history first, then hydrate only the model family needed for evaluation.",
    outcomes: [
      "Code, config, and artifact lineage share a ref",
      "Unchanged byte sequences can be reused across versions",
      "Named profiles make evaluation workspaces repeatable",
    ],
    command: "crab hydrate --profile=eval",
    commandLabel: "Materialize the evaluation set",
    docsHref: "/docs/cli/reference/crab-hydrate",
    docsLabel: "Explore hydration",
    visual: <ModelLineageVisual />,
  },
  {
    id: "data-research",
    navLabel: "Data & research",
    eyebrow: "Datasets and analysis",
    icon: Database,
    title: "Make the dataset snapshot part of the result.",
    description:
      "Version notebooks and their inputs together so a branch, tag, or release identifies the complete analytical state—not just the code around it.",
    challenge:
      "A notebook may be committed while its CSV, Parquet, imagery, or simulation output is replaced in place. The code is reproducible; the input path is not.",
    workflow:
      "Track selected data paths with Crab and commit their pointers beside the notebook. Each Git ref resolves to reconstruction metadata for the exact bytes. Researchers can inspect state with Crab, switch refs with Git, and hydrate only the paths required for the next analysis.",
    outcomes: [
      "Git refs identify exact data snapshots",
      "Selective hydrate avoids a full local copy",
      "Verified reconstruction returns the original bytes",
    ],
    command: "crab hydrate 'datasets/validation/**'",
    commandLabel: "Pull one research slice",
    docsHref: "/docs/cli/reference/crab-diff",
    docsLabel: "See data workflows",
    visual: <DataSnapshotVisual />,
  },
  {
    id: "creative-assets",
    navLabel: "Media & games",
    eyebrow: "Production asset libraries",
    icon: Film,
    title: "Open the project without downloading the archive.",
    description:
      "Keep textures, audio, video, CAD, and render outputs in the project history while each workstation materializes only its active set.",
    challenge:
      "Creative repositories grow faster than workstation disks. A fresh collaborator may need a few scenes or asset families—not every historical binary before the first edit.",
    workflow:
      "A lazy Crab clone checks out pointers instead of every managed payload. Artists can hydrate by path, use a named profile, or mount a repository for on-demand reads when a supported NFS or FUSE backend is available. Clean files can be dehydrated back to pointers to reclaim disk space.",
    outcomes: [
      "Lazy clone separates history from payload download",
      "Hydrate and dehydrate manage the working set",
      "Optional mounts expose files on demand",
    ],
    command: "crab dehydrate --all",
    commandLabel: "Reclaim local disk safely",
    docsHref: "/docs/cli/virtual-filesystem",
    docsLabel: "Explore virtual filesystems",
    visual: <CreativeWorkspaceVisual />,
  },
  {
    id: "ci-release",
    navLabel: "CI & release",
    eyebrow: "Automation and build inputs",
    icon: Workflow,
    title: "Give each job exactly the artifacts it needs.",
    description:
      "Keep large fixtures and release inputs on the same ref as the pipeline, then hydrate a deterministic manifest instead of pulling the entire repository payload.",
    challenge:
      "CI jobs often download a large shared fixture set even when a test shard touches only a handful of files. Ephemeral runners repeat that transfer unless the pipeline describes its working set.",
    workflow:
      "Crab supports named prefetch profiles and newline-delimited hydrate manifests. Jobs can pre-warm the cache without changing the checkout, materialize their paths, and use JSON or JSONL output for stable automation and diagnostics.",
    outcomes: [
      "Profiles keep job inputs reviewable",
      "Fetch can warm the cache before materialization",
      "Structured output fits CI logs and tooling",
    ],
    command: "crab hydrate --manifest .crab/manifests/ci.txt",
    commandLabel: "Hydrate a reviewed CI manifest",
    docsHref: "/docs/cli/automation",
    docsLabel: "Build an automation flow",
    visual: <CiProfileVisual />,
  },
  {
    id: "platform-security",
    navLabel: "Platform & security",
    eyebrow: "Direct object storage",
    icon: ShieldCheck,
    title: "Use the cloud boundary you already operate.",
    description:
      "For direct-storage repositories, developers and runners connect to your S3, GCS, Azure Blob, or S3-compatible bucket—without a Crab data server in the path.",
    challenge:
      "A new large-file service can introduce another data plane, credential model, scaling surface, and vendor boundary for the platform team to own.",
    workflow:
      "Crab discovers provider-native credentials such as AWS profiles and roles, Google Application Default Credentials, or Azure managed identities and SAS credentials. Git objects, refs, chunks, and reconstruction metadata live under the configured repository prefix in your bucket.",
    outcomes: [
      "Direct-storage mode has no Crab data server",
      "Provider-native credential chains stay in use",
      "Repository data remains in your chosen bucket",
    ],
    command: "crab doctor",
    commandLabel: "Verify auth, storage, and local setup",
    docsHref: "/docs/cli/authentication/configuration",
    docsLabel: "Review authentication",
    visual: <CloudBoundaryVisual />,
  },
]

const capabilityCards = [
  {
    icon: GitBranch,
    title: "Need exact lineage?",
    description:
      "Commit pointer blobs beside code so a Git ref identifies the complete project state.",
  },
  {
    icon: Layers3,
    title: "Large files change often?",
    description:
      "Content-defined chunks let new versions reuse byte sequences already stored in the repository.",
  },
  {
    icon: HardDrive,
    title: "Workstations are full?",
    description:
      "Clone lazily, hydrate selected paths, then dehydrate clean files when the task is done.",
  },
  {
    icon: Cloud,
    title: "Avoid another data server?",
    description:
      "Use direct-storage mode with the object store and credential chain your team already operates.",
  },
]

function ScenarioNav() {
  return (
    <nav
      aria-label="Use case navigation"
      className="sticky top-16 z-30 border-y border-border bg-background/80 backdrop-blur-xl supports-backdrop-filter:bg-background/70"
    >
      <div className="mx-auto flex max-w-6xl items-center gap-2 overflow-x-auto px-6 py-3">
        <span className="mr-2 hidden shrink-0 text-xs font-semibold tracking-[0.16em] text-muted-foreground uppercase lg:inline">
          Jump to
        </span>
        {useCases.map((useCase) => (
          <a
            key={useCase.id}
            href={"#" + useCase.id}
            className={cn(
              "shrink-0 rounded-full border border-transparent px-3 py-1.5 text-sm font-medium text-muted-foreground",
              "transition-colors hover:border-border hover:bg-muted hover:text-foreground",
              "focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none"
            )}
          >
            {useCase.navLabel}
          </a>
        ))}
      </div>
    </nav>
  )
}

function UseCaseSection({
  useCase,
  index,
}: {
  useCase: UseCase
  index: number
}) {
  const Icon = useCase.icon
  const visualFirst = index % 2 === 1

  return (
    <section
      id={useCase.id}
      aria-labelledby={useCase.id + "-heading"}
      className={cn(
        "scroll-mt-28 overflow-hidden border-b border-border py-20 md:py-28",
        index % 2 === 1 ? "bg-muted/30" : "bg-background"
      )}
    >
      <div className="mx-auto grid max-w-6xl items-center gap-12 px-6 lg:grid-cols-[0.92fr_1.08fr] lg:gap-16">
        <Reveal className={cn(visualFirst && "lg:order-2")}>
          <div className="flex items-center gap-3 text-primary">
            <span className="flex size-10 items-center justify-center rounded-xl border border-primary/20 bg-primary/10 shadow-sm">
              <Icon aria-hidden="true" size={20} />
            </span>
            <span className="text-xs font-semibold tracking-[0.16em] uppercase">
              {String(index + 1).padStart(2, "0")} · {useCase.eyebrow}
            </span>
          </div>

          <h2
            id={useCase.id + "-heading"}
            className="mt-6 text-3xl font-bold tracking-tight text-foreground md:text-4xl"
          >
            {useCase.title}
          </h2>
          <p className="mt-4 text-lg leading-8 text-muted-foreground">
            {useCase.description}
          </p>

          <div className="mt-8 space-y-5 border-l border-border pl-5">
            <div>
              <p className="text-xs font-semibold tracking-[0.14em] text-muted-foreground uppercase">
                Where teams get stuck
              </p>
              <p className="mt-2 leading-7 text-foreground/80">
                {useCase.challenge}
              </p>
            </div>
            <div>
              <p className="text-xs font-semibold tracking-[0.14em] text-primary uppercase">
                The Crab workflow
              </p>
              <p className="mt-2 leading-7 text-foreground/80">
                {useCase.workflow}
              </p>
            </div>
          </div>

          <ul className="mt-7 grid gap-3 text-sm text-foreground/80">
            {useCase.outcomes.map((outcome) => (
              <li key={outcome} className="flex items-start gap-3">
                <span className="mt-0.5 flex size-5 shrink-0 items-center justify-center rounded-full bg-primary/10 text-primary">
                  <Check aria-hidden="true" size={13} strokeWidth={2.5} />
                </span>
                {outcome}
              </li>
            ))}
          </ul>

          <Link
            href={useCase.docsHref}
            className="group mt-8 inline-flex items-center gap-2 text-sm font-semibold text-primary hover:text-primary-hover"
          >
            {useCase.docsLabel}
            <ArrowRight
              aria-hidden="true"
              size={16}
              className="transition-transform group-hover:translate-x-1"
            />
          </Link>
        </Reveal>

        <Reveal className={cn(!visualFirst && "lg:order-2")}>
          <div className="relative">
            <div
              aria-hidden="true"
              className="absolute -inset-8 -z-10 rounded-full bg-primary/8 blur-3xl"
            />
            {useCase.visual}
            <div className="mx-4 -mt-px flex flex-col gap-2 rounded-b-2xl border border-border bg-card/90 px-4 py-3 shadow-lg backdrop-blur sm:flex-row sm:items-center sm:justify-between">
              <div className="flex items-center gap-2 text-xs text-muted-foreground">
                <Terminal
                  aria-hidden="true"
                  size={14}
                  className="text-primary"
                />
                <span>{useCase.commandLabel}</span>
              </div>
              <code className="overflow-x-auto rounded-md bg-muted px-2.5 py-1.5 text-xs font-semibold whitespace-nowrap text-foreground">
                {useCase.command}
              </code>
            </div>
          </div>
        </Reveal>
      </div>
    </section>
  )
}

export default function UseCasesPage() {
  return (
    <MarketingLayout>
      <section className="relative overflow-hidden border-b border-border bg-background py-16 md:py-24">
        <div
          aria-hidden="true"
          className="absolute inset-0 [background-image:linear-gradient(to_right,var(--border)_1px,transparent_1px),linear-gradient(to_bottom,var(--border)_1px,transparent_1px)] [mask-image:linear-gradient(to_bottom,black,transparent_85%)] [background-size:48px_48px] opacity-40"
        />
        <div
          aria-hidden="true"
          className="absolute top-0 left-1/2 h-96 w-[46rem] -translate-x-1/2 rounded-full bg-primary/12 blur-3xl"
        />

        <div className="relative mx-auto grid max-w-6xl items-center gap-14 px-6 lg:grid-cols-[0.9fr_1.1fr]">
          <div>
            <Reveal>
              <Badge variant="secondary" className="rounded-full px-3 py-1">
                <span
                  aria-hidden="true"
                  className="mr-2 size-2 rounded-full bg-primary"
                />
                Built around the work, not the file type
              </Badge>
              <h1 className="mt-7 text-4xl font-bold tracking-tight text-foreground md:text-6xl md:leading-[1.05]">
                Large files belong in your workflow.{" "}
                <span className="bg-linear-to-r from-primary to-primary-hover bg-clip-text text-transparent">
                  Not beside it.
                </span>
              </h1>
              <p className="mt-6 max-w-xl text-lg leading-8 text-muted-foreground">
                Crab keeps Git as the timeline and your object store as the data
                plane. Version the whole project, move only the content a task
                needs, and keep every artifact tied to a commit.
              </p>
              <div className="mt-9 flex flex-wrap gap-3">
                <Button
                  size="lg"
                  render={<Link href="/docs/cli/getting-started" />}
                >
                  <BookOpen aria-hidden="true" />
                  Start with Crab
                </Button>
                <Button
                  variant="outline"
                  size="lg"
                  render={<Link href="/docs/cli/guides/migrating-from-lfs" />}
                >
                  Migrate from Git LFS
                  <ArrowRight aria-hidden="true" />
                </Button>
              </div>
              <div className="mt-9 flex flex-wrap gap-x-6 gap-y-3 text-sm text-muted-foreground">
                <span className="flex items-center gap-2">
                  <Box aria-hidden="true" size={16} className="text-primary" />
                  Pointer-based Git history
                </span>
                <span className="flex items-center gap-2">
                  <RefreshCcw
                    aria-hidden="true"
                    size={16}
                    className="text-primary"
                  />
                  Chunk-level reuse
                </span>
                <span className="flex items-center gap-2">
                  <Cloud
                    aria-hidden="true"
                    size={16}
                    className="text-primary"
                  />
                  Your cloud bucket
                </span>
              </div>
            </Reveal>
          </div>

          <Reveal>
            <UseCasesOverviewVisual />
          </Reveal>
        </div>
      </section>

      <ScenarioNav />

      <section
        aria-labelledby="find-your-fit"
        className="border-b border-border bg-muted/30 py-16 md:py-20"
      >
        <div className="mx-auto max-w-6xl px-6">
          <Reveal>
            <div className="flex flex-col justify-between gap-4 md:flex-row md:items-end">
              <div>
                <p className="text-xs font-semibold tracking-[0.16em] text-primary uppercase">
                  Find your fit
                </p>
                <h2
                  id="find-your-fit"
                  className="mt-3 text-3xl font-bold tracking-tight text-foreground"
                >
                  Start with the bottleneck you already have.
                </h2>
              </div>
              <p className="max-w-xl text-muted-foreground">
                Crab uses one storage model across every team: pointers in Git,
                content-addressed chunks in object storage, and explicit
                materialization in the workspace.
              </p>
            </div>
          </Reveal>

          <div className="mt-10 grid gap-4 md:grid-cols-2 lg:grid-cols-4">
            {capabilityCards.map((card, index) => {
              const Icon = card.icon
              return (
                <Reveal key={card.title} duration={360 + index * 70}>
                  <article className="group h-full rounded-2xl border border-border bg-card p-5 shadow-sm transition-all duration-300 hover:-translate-y-1 hover:border-primary/30 hover:shadow-card-hover">
                    <span className="flex size-10 items-center justify-center rounded-xl bg-primary/10 text-primary transition-transform duration-300 group-hover:scale-110">
                      <Icon aria-hidden="true" size={19} />
                    </span>
                    <h3 className="mt-5 font-semibold text-foreground">
                      {card.title}
                    </h3>
                    <p className="mt-2 text-sm leading-6 text-muted-foreground">
                      {card.description}
                    </p>
                  </article>
                </Reveal>
              )
            })}
          </div>
        </div>
      </section>

      {useCases.map((useCase, index) => (
        <UseCaseSection key={useCase.id} useCase={useCase} index={index} />
      ))}

      <CTASection
        variant="accent"
        headline="Bring the next large file into the commit."
        description="Configure a repository against your bucket, track the paths that matter, and ship code and content together."
        primaryCTA={{
          label: "Read the quickstart",
          href: "/docs/cli/getting-started",
          icon: BookOpen,
        }}
        secondaryCTA={{
          label: "Talk to us",
          href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog",
          icon: MailIcon,
        }}
      />
    </MarketingLayout>
  )
}
