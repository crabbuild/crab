"use client"

import {
  Check,
  Cloud,
  Code2,
  GitCommit,
  HardDrive,
  PackageOpen,
} from "lucide-react"
import { useState } from "react"

import { cn } from "@/lib/utils"

const stages = [
  {
    id: "track",
    label: "1. Track",
    action: "crab track '*.safetensors'",
    note: "The rule decides which path Crab manages. It does not create a second repository.",
    git: [
      "README.md → bytes",
      "train.py → bytes",
      "encoder.safetensors → not committed yet",
    ],
    bucket: ["No model data yet"],
    state: "Working tree knows the tracking rule.",
  },
  {
    id: "commit",
    label: "2. Commit",
    action: "git commit -m 'Add encoder'",
    note: "Git records one tree. The large path becomes a compact, verifiable pointer.",
    git: [
      "README.md → bytes",
      "train.py → bytes",
      "encoder.safetensors → Crab pointer",
    ],
    bucket: ["Chunks prepared locally", "Reconstruction recipe staged"],
    state: "One commit names code and the exact model version.",
  },
  {
    id: "push",
    label: "3. Push",
    action: "git push crab main",
    note: "Crab uploads missing chunks and metadata before the branch can move.",
    git: ["commit 8fc2", "tree → three paths", "model path → pointer f41a"],
    bucket: [
      "xorb → packed chunks",
      "shard → byte ranges",
      "recipe → ordered file",
    ],
    state: "main becomes visible only after its data is durable.",
  },
  {
    id: "hydrate",
    label: "4. Hydrate",
    action: "crab hydrate models/encoder.safetensors",
    note: "A new client reads the pointer, fetches only the required ranges, and verifies the rebuilt file.",
    git: ["checkout → pointer f41a", "history stays unchanged"],
    bucket: [
      "recipe resolves chunks",
      "ranges reconstruct 4 GB",
      "file hash verifies",
    ],
    state: "The working tree now has byte-identical model data.",
  },
] as const

export function CrabIntroductionWorkbench() {
  const [activeId, setActiveId] =
    useState<(typeof stages)[number]["id"]>("track")
  const active = stages.find((stage) => stage.id === activeId) ?? stages[0]

  return (
    <section className="wide-article-visual not-prose my-10 overflow-hidden rounded-2xl border border-border bg-card text-card-foreground shadow-sm lg:relative lg:left-1/2 lg:w-[min(72rem,calc(100vw-3rem))] lg:-translate-x-1/2">
      <header className="border-b border-border px-5 py-5 sm:px-7">
        <div className="flex flex-col gap-3 sm:flex-row sm:items-end sm:justify-between">
          <div>
            <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-primary">
              INTERACTIVE REPOSITORY WORKBENCH
            </p>
            <h2 className="m-0 mt-1 text-2xl font-black tracking-[-0.04em]">
              One commit. Two data paths.
            </h2>
          </div>
          <p className="m-0 max-w-sm text-sm leading-6 text-muted-foreground">
            Select a stage to see what Git stores, what the bucket stores, and
            which state becomes true.
          </p>
        </div>
      </header>

      <div
        className="grid border-b border-border bg-muted/40 sm:grid-cols-4"
        role="tablist"
        aria-label="Crab repository stages"
      >
        {stages.map((stage) => (
          <button
            key={stage.id}
            id={`crab-stage-${stage.id}`}
            type="button"
            role="tab"
            aria-selected={active.id === stage.id}
            aria-controls="crab-stage-panel"
            onClick={() => setActiveId(stage.id)}
            className={cn(
              "relative min-h-12 border-b border-border px-4 py-3 text-left font-mono text-xs font-black text-muted-foreground transition-colors outline-none after:absolute after:inset-x-4 after:bottom-0 after:h-0.5 after:scale-x-0 after:bg-primary after:transition-transform last:border-b-0 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset sm:border-r sm:border-b-0 sm:last:border-r-0",
              active.id === stage.id
                ? "bg-card text-card-foreground after:scale-x-100"
                : "hover:bg-card hover:text-card-foreground"
            )}
          >
            {stage.label}
          </button>
        ))}
      </div>

      <div
        id="crab-stage-panel"
        role="tabpanel"
        aria-labelledby={`crab-stage-${active.id}`}
        className="p-4 sm:p-6"
      >
        <div className="grid gap-4 xl:grid-cols-[15rem_minmax(0,1fr)_minmax(0,1fr)]">
          <div className="rounded-xl border border-border bg-muted/40 p-4">
            <div className="flex items-center gap-2 text-xs font-black tracking-[0.12em] text-muted-foreground uppercase">
              <Code2
                className="size-4 text-orange-600 dark:text-orange-400"
                aria-hidden="true"
              />
              Action
            </div>
            <code className="mt-4 block overflow-x-auto rounded-lg bg-[#142033] px-3 py-3 text-xs leading-5 text-white">
              {active.action}
            </code>
            <p className="m-0 mt-4 text-sm leading-6 text-muted-foreground">
              {active.note}
            </p>
          </div>

          <DataLane
            icon={GitCommit}
            eyebrow="GIT LANE"
            title="Commit graph"
            iconClassName="text-blue-600 dark:text-blue-400"
            dotClassName="bg-blue-600 dark:bg-blue-400"
            items={active.git}
          />
          <DataLane
            icon={Cloud}
            eyebrow="CRAB LANE"
            title="Object storage"
            iconClassName="text-orange-600 dark:text-orange-400"
            dotClassName="bg-orange-600 dark:bg-orange-400"
            items={active.bucket}
          />
        </div>

        <div className="mt-4 grid gap-3 border-t border-border pt-4 sm:grid-cols-[auto_minmax(0,1fr)_auto] sm:items-center">
          <span className="flex size-10 items-center justify-center rounded-full bg-emerald-50 text-emerald-700 dark:bg-emerald-950/50 dark:text-emerald-300">
            <Check className="size-5" aria-hidden="true" />
          </span>
          <div>
            <p className="m-0 font-mono text-[9px] font-black tracking-[0.18em] text-emerald-700 dark:text-emerald-300">
              STATE NOW TRUE
            </p>
            <p aria-live="polite" className="m-0 mt-1 text-sm font-bold">
              {active.state}
            </p>
          </div>
          <div className="flex items-center gap-2 text-xs font-bold text-emerald-700 dark:text-emerald-300">
            {active.id === "hydrate" ? (
              <PackageOpen className="size-4" aria-hidden="true" />
            ) : (
              <HardDrive className="size-4" aria-hidden="true" />
            )}
            {active.id === "hydrate" ? "BYTES LOCAL" : "HISTORY INTACT"}
          </div>
        </div>
      </div>
    </section>
  )
}

function DataLane({
  icon: Icon,
  eyebrow,
  title,
  iconClassName,
  dotClassName,
  items,
}: {
  icon: typeof GitCommit
  eyebrow: string
  title: string
  iconClassName: string
  dotClassName: string
  items: readonly string[]
}) {
  return (
    <section className="overflow-hidden rounded-xl border border-border bg-card">
      <header className="flex items-center gap-3 border-b border-border px-4 py-3">
        <Icon className={cn("size-5", iconClassName)} aria-hidden="true" />
        <div>
          <p className="m-0 font-mono text-[9px] font-black tracking-[0.18em] text-muted-foreground">
            {eyebrow}
          </p>
          <h3 className="m-0 mt-0.5 text-sm font-black">{title}</h3>
        </div>
      </header>
      <ul className="m-0 grid list-none divide-y divide-border p-0">
        {items.map((item) => (
          <li
            key={item}
            className="flex min-h-11 items-center gap-3 px-4 py-3 text-xs leading-5"
          >
            <span
              className={cn("size-2 shrink-0 rounded-sm", dotClassName)}
              aria-hidden="true"
            />
            {item}
          </li>
        ))}
      </ul>
    </section>
  )
}
