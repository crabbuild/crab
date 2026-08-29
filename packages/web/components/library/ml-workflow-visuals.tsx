"use client"

import { useState } from "react"
import {
  CheckCircle2,
  GitCompareArrows,
  Play,
  RotateCcw,
  ShieldCheck,
} from "lucide-react"

import { DiagramFrame } from "@/components/blog/blog-diagrams"
import { Button } from "@/components/ui/button"
import { cn } from "@/lib/utils"

type StageState = "run" | "cache"

type Scenario = {
  id: string
  label: string
  change: string
  command: string
  reason: string
  stages: StageState[]
}

const STAGES = [
  { name: "ingest", output: "raw.parquet" },
  { name: "features", output: "features.parquet" },
  { name: "train", output: "model.pkl" },
  { name: "evaluate", output: "metrics.json" },
] as const

const SCENARIOS: Scenario[] = [
  {
    id: "first-run",
    label: "First run",
    change: "No cache exists yet",
    command: "crab run --cache-push",
    reason:
      "Every content-addressed stage key is new, so the complete DAG executes.",
    stages: ["run", "run", "run", "run"],
  },
  {
    id: "same-inputs",
    label: "Same inputs",
    change: "Code, data, and params are unchanged",
    command: "crab run",
    reason:
      "All four stage keys match. Crab restores or keeps their recorded outputs.",
    stages: ["cache", "cache", "cache", "cache"],
  },
  {
    id: "training-code",
    label: "Train code changed",
    change: "src/train.py has new bytes",
    command: "crab run --dry --explain-miss",
    reason:
      "Ingest and features still match. Train changes, so its downstream evaluation changes too.",
    stages: ["cache", "cache", "run", "run"],
  },
  {
    id: "raw-data",
    label: "Dataset changed",
    change: "data/transactions.csv has new rows",
    command: "crab run --downstream ingest",
    reason:
      "The first stage key changes and invalidation follows every producer-to-consumer edge.",
    stages: ["run", "run", "run", "run"],
  },
]

const STATE_STYLE: Record<
  StageState,
  { fill: string; stroke: string; label: string }
> = {
  run: {
    fill: "color-mix(in srgb, #f97316 12%, var(--card))",
    stroke: "#f97316",
    label: "RUN",
  },
  cache: {
    fill: "color-mix(in srgb, #10b981 12%, var(--card))",
    stroke: "#10b981",
    label: "CACHE HIT",
  },
}

export function MlWorkflowCacheExplorer() {
  const [activeId, setActiveId] = useState(SCENARIOS[0].id)
  const active =
    SCENARIOS.find((scenario) => scenario.id === activeId) ?? SCENARIOS[0]
  const runCount = active.stages.filter((state) => state === "run").length

  return (
    <DiagramFrame
      title="Explore content-addressed invalidation"
      caption="Choose a real workflow change. Orange stages execute; green stages reuse their matching result. The selected scenario changes only the simulated explanation—no command runs in your browser."
      className="wide-article-visual"
    >
      <div className="grid gap-5 lg:grid-cols-[14rem_minmax(0,1fr)]">
        <div
          className="grid gap-2 sm:grid-cols-2 lg:grid-cols-1"
          aria-label="Workflow change scenarios"
        >
          {SCENARIOS.map((scenario) => (
            <Button
              key={scenario.id}
              type="button"
              variant={scenario.id === active.id ? "default" : "outline"}
              size="lg"
              aria-pressed={scenario.id === active.id}
              onClick={() => setActiveId(scenario.id)}
              className="min-h-11 justify-start px-3 text-left"
            >
              {scenario.id === "same-inputs" ? <RotateCcw /> : <Play />}
              {scenario.label}
            </Button>
          ))}
        </div>

        <div className="min-w-0">
          <div
            className="overflow-x-auto pb-2"
            role="region"
            aria-label={`${active.label} stage plan`}
            tabIndex={0}
          >
            <svg
              viewBox="0 0 820 190"
              className="h-auto w-full min-w-[44rem]"
              role="img"
              aria-label={`${active.label}: ${runCount} stages run and ${STAGES.length - runCount} use cache`}
            >
              <defs>
                <marker
                  id="ml-workflow-arrow"
                  viewBox="0 0 10 10"
                  refX="9"
                  refY="5"
                  markerWidth="8"
                  markerHeight="8"
                  orient="auto"
                >
                  <path d="M1 1 9 5 1 9 3.5 5Z" fill="#64748b" />
                </marker>
              </defs>
              {STAGES.slice(0, -1).map((stage, index) => (
                <line
                  key={`${stage.name}-edge`}
                  x1={180 + index * 200}
                  y1="82"
                  x2={220 + index * 200}
                  y2="82"
                  stroke="#64748b"
                  strokeWidth="2"
                  markerEnd="url(#ml-workflow-arrow)"
                />
              ))}
              {STAGES.map((stage, index) => {
                const state = active.stages[index]
                const style = STATE_STYLE[state]
                const x = 20 + index * 200
                return (
                  <g key={stage.name}>
                    <rect
                      x={x}
                      y="35"
                      width="160"
                      height="94"
                      rx="12"
                      fill={style.fill}
                      stroke={style.stroke}
                      strokeWidth="2"
                    />
                    <text
                      x={x + 80}
                      y="66"
                      textAnchor="middle"
                      fill="var(--foreground)"
                      fontSize="14"
                      fontWeight="700"
                    >
                      {stage.name}
                    </text>
                    <text
                      x={x + 80}
                      y="88"
                      textAnchor="middle"
                      fill="var(--muted-foreground)"
                      fontFamily="ui-monospace, monospace"
                      fontSize="10"
                    >
                      {stage.output}
                    </text>
                    <text
                      x={x + 80}
                      y="113"
                      textAnchor="middle"
                      fill={style.stroke}
                      fontSize="10"
                      fontWeight="750"
                    >
                      {style.label}
                    </text>
                  </g>
                )
              })}
              <text
                x="410"
                y="166"
                textAnchor="middle"
                fill="var(--muted-foreground)"
                fontSize="11"
              >
                Stage key = command + dependencies + declared parameters +
                selected environment
              </text>
            </svg>
          </div>

          <div className="mt-3 grid gap-3 rounded-lg border border-border bg-muted/25 p-4 sm:grid-cols-[minmax(0,1fr)_auto]">
            <div>
              <div className="flex items-center gap-2 text-sm font-semibold text-foreground">
                <CheckCircle2 className="text-primary" size={16} />
                {active.change}
              </div>
              <p className="mt-2 text-xs leading-5 text-muted-foreground">
                {active.reason}
              </p>
            </div>
            <div className="self-center rounded-md border border-border bg-background px-3 py-2 font-mono text-[11px] text-foreground">
              {active.command}
            </div>
          </div>

          <div className="mt-3 flex flex-wrap gap-2 text-xs">
            <span
              className={cn(
                "rounded-full px-3 py-1 font-medium",
                runCount > 0
                  ? "bg-orange-500/10 text-orange-700 dark:text-orange-300"
                  : "bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
              )}
            >
              {runCount} executed
            </span>
            <span className="rounded-full bg-emerald-500/10 px-3 py-1 font-medium text-emerald-700 dark:text-emerald-300">
              {STAGES.length - runCount} reused
            </span>
          </div>
        </div>
      </div>
    </DiagramFrame>
  )
}

const SWEEP_ROWS = [
  { lr: "0.01", results: [0.781, 0.804, 0.798] },
  { lr: "0.05", results: [0.812, 0.841, 0.833] },
  { lr: "0.10", results: [0.793, 0.818, 0.806] },
] as const
const DEPTHS = [4, 8, 12] as const

export function MlExperimentSweepExplorer() {
  const [selected, setSelected] = useState({ row: 1, column: 1 })
  const row = SWEEP_ROWS[selected.row]
  const depth = DEPTHS[selected.column]
  const recall = row.results[selected.column]
  const precision = Math.min(0.94, recall + 0.071)

  return (
    <DiagramFrame
      title="Inspect a nine-run parameter sweep"
      caption="Select a result to inspect its illustrative metrics. Crab records the real parameter overrides, stage hashes, and metrics for each experiment; this browser grid is an explanation, not an experiment runner."
      className="wide-article-visual"
    >
      <div className="grid gap-6 lg:grid-cols-[minmax(0,1fr)_17rem]">
        <div className="min-w-0">
          <div
            className="grid grid-cols-[4.5rem_repeat(3,minmax(4.5rem,1fr))] gap-2"
            role="grid"
            aria-label="Recall by learning rate and tree depth"
          >
            <div />
            {DEPTHS.map((value) => (
              <div
                key={value}
                className="px-2 py-1 text-center text-[11px] font-semibold text-muted-foreground"
              >
                depth {value}
              </div>
            ))}
            {SWEEP_ROWS.map((sweepRow, rowIndex) => (
              <div key={sweepRow.lr} className="contents">
                <div className="flex items-center text-[11px] font-semibold text-muted-foreground">
                  lr {sweepRow.lr}
                </div>
                {sweepRow.results.map((value, columnIndex) => {
                  const isSelected =
                    selected.row === rowIndex && selected.column === columnIndex
                  const strength = Math.round((value - 0.76) * 550)
                  return (
                    <button
                      key={`${sweepRow.lr}-${DEPTHS[columnIndex]}`}
                      type="button"
                      role="gridcell"
                      aria-selected={isSelected}
                      aria-label={`Learning rate ${sweepRow.lr}, depth ${DEPTHS[columnIndex]}, recall ${value.toFixed(3)}`}
                      onClick={() =>
                        setSelected({ row: rowIndex, column: columnIndex })
                      }
                      className={cn(
                        "min-h-16 rounded-lg border px-2 py-3 text-center transition-all focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none",
                        isSelected
                          ? "border-primary ring-2 ring-primary/20"
                          : "border-border hover:border-primary/50"
                      )}
                      style={{
                        background: `color-mix(in srgb, #10b981 ${Math.max(8, strength)}%, var(--card))`,
                      }}
                    >
                      <span className="block text-sm font-bold text-foreground">
                        {value.toFixed(3)}
                      </span>
                      <span className="mt-1 block text-[10px] text-muted-foreground">
                        recall
                      </span>
                    </button>
                  )
                })}
              </div>
            ))}
          </div>
          <div className="mt-4 rounded-md border border-border bg-muted/25 px-3 py-2 font-mono text-[11px] text-foreground">
            crab exp show lr-{row.lr.replace(".", "-")}-d{depth} --json
          </div>
        </div>

        <aside className="rounded-lg border border-border bg-card p-4">
          <div className="text-[11px] font-semibold tracking-wide text-muted-foreground uppercase">
            Selected experiment
          </div>
          <div className="mt-3 text-lg font-bold text-foreground">
            lr {row.lr} · depth {depth}
          </div>
          <dl className="mt-4 space-y-3 text-sm">
            <MetricRow
              label="Recall"
              value={recall.toFixed(3)}
              best={recall === 0.841}
            />
            <MetricRow label="Precision" value={precision.toFixed(3)} />
            <MetricRow
              label="Runs reused"
              value={selected.row === 1 ? "2 / 4" : "1 / 4"}
            />
          </dl>
          <p className="mt-4 text-xs leading-5 text-muted-foreground">
            Compare the candidate against the baseline before applying files or
            creating a review branch.
          </p>
        </aside>
      </div>
    </DiagramFrame>
  )
}

function MetricRow({
  label,
  value,
  best = false,
}: {
  label: string
  value: string
  best?: boolean
}) {
  return (
    <div className="flex items-center justify-between gap-3">
      <dt className="text-muted-foreground">{label}</dt>
      <dd
        className={cn(
          "font-mono font-semibold",
          best ? "text-emerald-600 dark:text-emerald-400" : "text-foreground"
        )}
      >
        {value}
        {best ? " · best" : ""}
      </dd>
    </div>
  )
}

const VERSIONS = [
  { id: "b3:8a41…", name: "v1", recall: "0.812", commit: "3fd21a" },
  { id: "b3:c920…", name: "v2", recall: "0.833", commit: "8bc014" },
  { id: "b3:f311…", name: "v3", recall: "0.841", commit: "cb760e" },
] as const

export function ArtifactPromotionLab() {
  const [stage, setStage] = useState<"staging" | "production">("production")
  const [candidate, setCandidate] = useState(2)
  const [concurrent, setConcurrent] = useState(false)
  const expected = stage === "production" ? VERSIONS[1] : VERSIONS[2]
  const observed = concurrent ? VERSIONS[2] : expected
  const succeeds = observed.id === expected.id

  return (
    <DiagramFrame
      title="Preview an immutable artifact promotion"
      caption="Versions never move. A stage label is a mutable pointer updated with compare-and-swap. Toggle the concurrent change to see why automation passes --expected."
      className="wide-article-visual"
    >
      <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_18rem]">
        <div className="min-w-0">
          <div className="flex flex-wrap gap-2">
            {VERSIONS.map((version, index) => (
              <Button
                key={version.id}
                type="button"
                size="lg"
                variant={candidate === index ? "default" : "outline"}
                aria-pressed={candidate === index}
                onClick={() => setCandidate(index)}
                className="min-h-11"
              >
                {version.name} · recall {version.recall}
              </Button>
            ))}
          </div>

          <div
            className="mt-4 overflow-x-auto"
            role="region"
            aria-label="Immutable versions and mutable stage labels"
            tabIndex={0}
          >
            <svg
              viewBox="0 0 700 260"
              className="h-auto w-full min-w-[38rem]"
              role="img"
              aria-label={`${stage} points to ${observed.name}; candidate is ${VERSIONS[candidate].name}`}
            >
              <defs>
                <marker
                  id="artifact-label-arrow"
                  viewBox="0 0 10 10"
                  refX="9"
                  refY="5"
                  markerWidth="8"
                  markerHeight="8"
                  orient="auto"
                >
                  <path d="M1 1 9 5 1 9 3.5 5Z" fill="#8b5cf6" />
                </marker>
              </defs>
              <text
                x="145"
                y="26"
                textAnchor="middle"
                fill="var(--muted-foreground)"
                fontSize="11"
                fontWeight="700"
              >
                IMMUTABLE VERSIONS
              </text>
              <text
                x="555"
                y="26"
                textAnchor="middle"
                fill="var(--muted-foreground)"
                fontSize="11"
                fontWeight="700"
              >
                MUTABLE LABEL
              </text>
              {VERSIONS.map((version, index) => (
                <g key={version.id}>
                  <rect
                    x="30"
                    y={48 + index * 66}
                    width="230"
                    height="50"
                    rx="9"
                    fill={
                      candidate === index
                        ? "color-mix(in srgb, #06b6d4 12%, var(--card))"
                        : "var(--card)"
                    }
                    stroke={candidate === index ? "#06b6d4" : "var(--border)"}
                    strokeWidth="2"
                  />
                  <text
                    x="48"
                    y={69 + index * 66}
                    fill="var(--foreground)"
                    fontSize="12"
                    fontWeight="700"
                  >
                    {version.name} · {version.id}
                  </text>
                  <text
                    x="48"
                    y={87 + index * 66}
                    fill="var(--muted-foreground)"
                    fontFamily="ui-monospace, monospace"
                    fontSize="9"
                  >
                    commit {version.commit} · recall {version.recall}
                  </text>
                </g>
              ))}
              <line
                x1="470"
                y1="129"
                x2="275"
                y2={73 + VERSIONS.indexOf(observed) * 66}
                stroke="#8b5cf6"
                strokeWidth="2.5"
                markerEnd="url(#artifact-label-arrow)"
              />
              <rect
                x="470"
                y="92"
                width="190"
                height="74"
                rx="12"
                fill="color-mix(in srgb, #8b5cf6 10%, var(--card))"
                stroke="#8b5cf6"
                strokeWidth="2"
              />
              <text
                x="565"
                y="121"
                textAnchor="middle"
                fill="var(--foreground)"
                fontSize="14"
                fontWeight="700"
              >
                {stage}
              </text>
              <text
                x="565"
                y="145"
                textAnchor="middle"
                fill="#8b5cf6"
                fontFamily="ui-monospace, monospace"
                fontSize="10"
              >
                → {observed.name} · {observed.id}
              </text>
            </svg>
          </div>
        </div>

        <aside className="rounded-lg border border-border bg-card p-4">
          <div className="flex gap-2">
            {(["staging", "production"] as const).map((value) => (
              <Button
                key={value}
                type="button"
                variant={stage === value ? "secondary" : "ghost"}
                size="lg"
                onClick={() => setStage(value)}
                className="min-h-11 flex-1"
              >
                {value}
              </Button>
            ))}
          </div>
          <Button
            type="button"
            variant={concurrent ? "destructive" : "outline"}
            size="lg"
            onClick={() => setConcurrent((value) => !value)}
            className="mt-3 min-h-11 w-full"
          >
            <GitCompareArrows />
            {concurrent ? "Concurrent change on" : "Simulate concurrent change"}
          </Button>

          <div
            className={cn(
              "mt-4 rounded-lg border p-3",
              succeeds
                ? "border-emerald-500/30 bg-emerald-500/10"
                : "border-destructive/30 bg-destructive/10"
            )}
          >
            <div className="flex items-center gap-2 text-sm font-semibold text-foreground">
              <ShieldCheck
                size={16}
                className={succeeds ? "text-emerald-600" : "text-destructive"}
              />
              {succeeds ? "CAS can succeed" : "CAS rejects stale writer"}
            </div>
            <p className="mt-2 text-xs leading-5 text-muted-foreground">
              Expected {expected.name}; registry currently reports{" "}
              {observed.name}.
            </p>
          </div>

          <div className="mt-3 overflow-x-auto rounded-md border border-border bg-muted/25 p-3 font-mono text-[10px] leading-5 text-foreground">
            crab artifacts promote fraud-model {VERSIONS[candidate].id} {stage}{" "}
            --expected {expected.id}
          </div>
        </aside>
      </div>
    </DiagramFrame>
  )
}
