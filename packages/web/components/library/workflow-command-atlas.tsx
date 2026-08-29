"use client"

import { useState } from "react"
import {
  BarChart3,
  FlaskConical,
  ListChecks,
  Package,
  Play,
  Workflow,
} from "lucide-react"
import type { LucideIcon } from "lucide-react"

import { DiagramFrame } from "@/components/blog/blog-diagrams"
import { Button } from "@/components/ui/button"

type CommandFamily = {
  id: string
  label: string
  icon: LucideIcon
  purpose: string
  commands: { command: string; detail: string }[]
}

const COMMAND_FAMILIES: CommandFamily[] = [
  {
    id: "run",
    label: "Run & author",
    icon: Play,
    purpose: "Declare stages and control execution.",
    commands: [
      { command: "crab run", detail: "Execute or replay declared stages" },
      {
        command: "crab repro",
        detail: "DVC-compatible workflow execution alias",
      },
      {
        command: "crab stage add",
        detail: "Create or replace one stage declaration",
      },
      { command: "crab stage list", detail: "List discovered stages" },
      { command: "crab freeze", detail: "Skip selected stages until unfrozen" },
      { command: "crab unfreeze", detail: "Restore normal invalidation" },
    ],
  },
  {
    id: "workflow",
    label: "Workflow state",
    icon: Workflow,
    purpose: "Inspect DAG, locks, journals, and shared cache.",
    commands: [
      { command: "crab workflow status", detail: "Report stage freshness" },
      {
        command: "crab workflow dag",
        detail: "Render the producer-consumer graph",
      },
      {
        command: "crab workflow lockfile resolve",
        detail: "Resolve a conflicted lockfile",
      },
      {
        command: "crab workflow lockfile split",
        detail: "Migrate to per-workflow locks",
      },
      { command: "crab workflow journal ls", detail: "List run trajectories" },
      {
        command: "crab workflow journal show",
        detail: "Inspect one run trajectory",
      },
      {
        command: "crab workflow journal gc",
        detail: "Prune old terminal journals",
      },
      {
        command: "crab workflow push-cache",
        detail: "Publish local stage entries",
      },
    ],
  },
  {
    id: "experiments",
    label: "Experiments",
    icon: FlaskConical,
    purpose: "Run, compare, share, and maintain isolated experiments.",
    commands: [
      { command: "crab exp run", detail: "Execute an isolated experiment" },
      {
        command: "crab exp show",
        detail: "Inspect one experiment or recent runs",
      },
      {
        command: "crab exp diff",
        detail: "Compare params, hashes, and metrics",
      },
      { command: "crab exp ls", detail: "List local experiments" },
      { command: "crab exp promote", detail: "Create a Git review branch" },
      {
        command: "crab exp apply",
        detail: "Apply a snapshot to the workspace",
      },
      { command: "crab exp reset", detail: "Reset checkpoint lineage" },
      { command: "crab exp save", detail: "Capture the current workspace" },
      { command: "crab exp rename", detail: "Change a human label" },
      { command: "crab exp push", detail: "Publish experiment metadata" },
      { command: "crab exp pull", detail: "Retrieve experiment metadata" },
      { command: "crab exp remove", detail: "Remove selected local metadata" },
      { command: "crab exp clean", detail: "Clean temporary experiment state" },
      { command: "crab exp gc", detail: "Prune older metadata" },
      { command: "crab exp queue", detail: "Create queued parameter tasks" },
      { command: "crab exp start", detail: "Start queued workers" },
      { command: "crab exp status", detail: "Inspect queue state" },
      { command: "crab exp stop", detail: "Stop workers gracefully" },
    ],
  },
  {
    id: "queue",
    label: "Queue",
    icon: ListChecks,
    purpose: "Operate local experiment tasks and workers.",
    commands: [
      { command: "crab queue start", detail: "Start bounded workers" },
      { command: "crab queue status", detail: "List tasks and workers" },
      { command: "crab queue logs", detail: "Read task output" },
      { command: "crab queue kill", detail: "Interrupt selected tasks" },
      { command: "crab queue remove", detail: "Remove non-running tasks" },
      {
        command: "crab queue stop",
        detail: "Request graceful worker shutdown",
      },
    ],
  },
  {
    id: "evidence",
    label: "Evidence",
    icon: BarChart3,
    purpose: "Read parameters and compare model evidence.",
    commands: [
      { command: "crab params show", detail: "Read flattened parameters" },
      { command: "crab params diff", detail: "Compare parameter values" },
      { command: "crab metrics show", detail: "Read recorded metrics" },
      { command: "crab metrics diff", detail: "Compare scalar outcomes" },
      { command: "crab metrics plot", detail: "Render declared plots" },
      { command: "crab plots show", detail: "Render current plot sources" },
      { command: "crab plots diff", detail: "Overlay plot revisions" },
      {
        command: "crab plots templates",
        detail: "Inspect Vega-Lite templates",
      },
    ],
  },
  {
    id: "artifacts",
    label: "Artifacts",
    icon: Package,
    purpose: "Version, retrieve, and promote immutable model bytes.",
    commands: [
      { command: "crab artifacts list", detail: "List catalog state" },
      { command: "crab artifacts show", detail: "Inspect one artifact" },
      { command: "crab artifacts get", detail: "Retrieve a verified payload" },
      {
        command: "crab artifacts version create",
        detail: "Capture immutable bytes",
      },
      {
        command: "crab artifacts promote",
        detail: "Move a stage label with CAS",
      },
      { command: "crab artifacts history", detail: "Inspect promotion events" },
    ],
  },
]

export function WorkflowCommandAtlas() {
  const [activeId, setActiveId] = useState(COMMAND_FAMILIES[0].id)
  const active =
    COMMAND_FAMILIES.find((family) => family.id === activeId) ??
    COMMAND_FAMILIES[0]
  const total = COMMAND_FAMILIES.reduce(
    (count, family) => count + family.commands.length,
    0
  )

  return (
    <DiagramFrame
      title="Explore the complete public workflow CLI"
      caption={`${total} public command forms across six families. Select a family to inspect its operator boundary; the internal checkpoint control protocol is intentionally excluded.`}
      className="wide-article-visual"
    >
      <div className="space-y-4">
        <div
          className="overflow-x-auto pb-2"
          aria-label="Workflow command families"
        >
          <div className="flex min-w-max gap-2">
            {COMMAND_FAMILIES.map((family) => {
              const Icon = family.icon

              return (
                <Button
                  key={family.id}
                  type="button"
                  size="lg"
                  variant={family.id === active.id ? "default" : "outline"}
                  aria-pressed={family.id === active.id}
                  onClick={() => setActiveId(family.id)}
                  className="min-h-11 shrink-0 gap-2 px-3"
                >
                  <Icon size={15} aria-hidden="true" />
                  <span>{family.label}</span>
                  <span className="rounded-full bg-background/15 px-1.5 font-mono text-[10px]">
                    {family.commands.length}
                  </span>
                </Button>
              )
            })}
          </div>
        </div>

        <section className="min-w-0 rounded-lg border border-border bg-card">
          <header className="border-b border-border bg-muted/25 px-4 py-3">
            <div className="flex items-center justify-between gap-3">
              <div>
                <h3 className="m-0 text-sm font-semibold text-foreground">
                  {active.label}
                </h3>
                <p className="m-0 mt-1 text-xs text-muted-foreground">
                  {active.purpose}
                </p>
              </div>
              <span className="rounded-full bg-primary/10 px-2.5 py-1 text-xs font-semibold text-primary">
                {active.commands.length} commands
              </span>
            </div>
          </header>
          <div className="grid gap-px bg-border sm:grid-cols-2">
            {active.commands.map((item) => (
              <div key={item.command} className="min-w-0 bg-card px-4 py-3">
                <code className="block overflow-x-auto text-[11px] font-semibold text-foreground">
                  {item.command}
                </code>
                <p className="m-0 mt-1 text-xs leading-5 text-muted-foreground">
                  {item.detail}
                </p>
              </div>
            ))}
          </div>
        </section>
      </div>
    </DiagramFrame>
  )
}
