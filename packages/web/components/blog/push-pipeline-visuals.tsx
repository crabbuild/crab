"use client"

import {
  AlertTriangle,
  ArrowRight,
  Check,
  Cloud,
  GitBranch,
  PackageCheck,
  ShieldCheck,
} from "lucide-react"
import { useState } from "react"

import { cn } from "@/lib/utils"

type PhaseName = "Plan" | "Upload" | "Prove" | "Publish"

type PipelineStage = {
  number: number
  phase: PhaseName
  title: string
  action: string
  evidence: string
  structure: string
  fields: string[]
  before: string
  after: string
}

const PHASES: Array<{
  name: PhaseName
  range: string
  color: string
  width: string
}> = [
  { name: "Plan", range: "01–04", color: "#6557c8", width: "28.57%" },
  { name: "Upload", range: "05–10", color: "#087f73", width: "42.86%" },
  { name: "Prove", range: "11–12", color: "#bd642e", width: "14.29%" },
  { name: "Publish", range: "13–14", color: "#d3423f", width: "14.29%" },
]

const PHASE_COLOR = Object.fromEntries(
  PHASES.map((phase) => [phase.name, phase.color])
) as Record<PhaseName, string>

const PIPELINE_STAGES: PipelineStage[] = [
  {
    number: 1,
    phase: "Plan",
    title: "Parse refspec",
    action: "Resolve the source and destination ref.",
    evidence: "Intended ref edit",
    structure: "PushSpec",
    fields: ["src", "dst", "force"],
    before: "main:main",
    after: "validated ref edit",
  },
  {
    number: 2,
    phase: "Plan",
    title: "Read destination",
    action: "Record the current tip of main.",
    evidence: "Expected-old A",
    structure: "RefJournalHeadSnapshot",
    fields: ["head", "etag", "visible_transaction"],
    before: "remote tip unknown",
    after: "expected-old = A",
  },
  {
    number: 3,
    phase: "Plan",
    title: "Acquire lock",
    action: "Serialize work for this destination ref.",
    evidence: "Writer lease",
    structure: "PushLockLease",
    fields: ["lock", "heartbeat"],
    before: "main unlocked",
    after: "lease held",
  },
  {
    number: 4,
    phase: "Plan",
    title: "Discover closure",
    action: "Walk Git objects and every Crab pointer.",
    evidence: "Dependency plan",
    structure: "PushDependencyPlan",
    fields: ["ref_edits", "file_dependencies", "unique_chunks"],
    before: "commit roots",
    after: "complete dependency set",
  },
  {
    number: 5,
    phase: "Upload",
    title: "Classify chunks",
    action: "Separate proven chunks from new content.",
    evidence: "Upload set",
    structure: "FilePushPlan",
    fields: ["chunks", "existing", "prepared_xorbs"],
    before: "chunks unclassified",
    after: "reuse + upload sets",
  },
  {
    number: 6,
    phase: "Upload",
    title: "Build Git pack",
    action: "Pack commits, trees, blobs, and pointers.",
    evidence: "Git transport object",
    structure: "PreparedGitPack",
    fields: ["refs", "exact_missing_objects", "all_candidates_proven"],
    before: "reachable Git objects",
    after: "pack candidate",
  },
  {
    number: 7,
    phase: "Upload",
    title: "Build xorbs",
    action: "Compress new chunks into immutable objects.",
    evidence: "Xorb objects",
    structure: "PackedXorb",
    fields: ["hash", "placements", "payload"],
    before: "new chunk stream",
    after: "content-addressed xorbs",
  },
  {
    number: 8,
    phase: "Upload",
    title: "Upload xorbs",
    action: "Flush staging and store new chunk bytes.",
    evidence: "Durable large-file data",
    structure: "UploadedXorb",
    fields: ["hash", "len", "payload"],
    before: "local xorb payload",
    after: "durable origin object",
  },
  {
    number: 9,
    phase: "Upload",
    title: "Upload Git pack",
    action: "Store the standard Git representation.",
    evidence: "Durable Git objects",
    structure: "UploadedGitPack",
    fields: ["entry", "idx_path", "git_sha1"],
    before: "local pack candidate",
    after: "durable pack entry",
  },
  {
    number: 10,
    phase: "Upload",
    title: "Publish shards",
    action: "Store complete file reconstruction terms.",
    evidence: "Durable file map",
    structure: "PushShardSession",
    fields: ["writers", "current", "current_hashes"],
    before: "file recipes in memory",
    after: "uploaded shard hashes",
  },
  {
    number: 11,
    phase: "Prove",
    title: "Verify Git",
    action: "Prove the proposed commit graph is complete.",
    evidence: "Git closure",
    structure: "GitObjectSetProof",
    fields: ["count", "digest"],
    before: "candidate pack inventory",
    after: "Git closure proven",
  },
  {
    number: 12,
    phase: "Prove",
    title: "Verify pointers",
    action: "Prove every pointer reaches durable bytes.",
    evidence: "Pointer closure",
    structure: "PushCommitReceipt",
    fields: ["file_recipe_set_digest", "xorb_proof_digest", "plan_digest"],
    before: "candidate placements",
    after: "pointer closure proven",
  },
  {
    number: 13,
    phase: "Publish",
    title: "Compare tip",
    action: "Confirm main still equals expected-old A.",
    evidence: "Freshness decision",
    structure: "RefUpdateDecision",
    fields: ["Proceed { etag }", "Reject(reason)"],
    before: "expected A · current ?",
    after: "proceed or reject",
  },
  {
    number: 14,
    phase: "Publish",
    title: "Commit ref",
    action: "Append the transaction that moves main to B.",
    evidence: "Visible ref B",
    structure: "RefJournalTransaction",
    fields: ["parents", "edits", "packs", "shards"],
    before: "committed head = A",
    after: "committed head = B",
  },
]

export function PushPipelineBoard() {
  const [activeNumber, setActiveNumber] = useState(1)
  const active = PIPELINE_STAGES[activeNumber - 1]
  const published = active.number === 14
  const color = PHASE_COLOR[active.phase]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden border-2 border-[#17231e] bg-[#edf4f1] shadow-[7px_7px_0_#17231e] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="flex flex-wrap items-end justify-between gap-4 border-b-2 border-[#17231e] bg-[#fbfcf9] px-4 py-4 sm:px-6">
        <div>
          <p className="m-0 font-mono text-[10px] font-bold tracking-[0.2em] text-[#52615a]">
            PUSH CONTROL SHEET · CLICK ANY STAGE
          </p>
          <h3 className="m-0 mt-1 text-xl font-black tracking-[-0.035em] text-[#17231e] sm:text-2xl">
            Fourteen checks. One visible change.
          </h3>
        </div>
        <div className="flex items-center gap-2 font-mono text-[11px] font-bold text-[#17231e]">
          <span className="size-2 bg-[#d3423f]" aria-hidden="true" />
          REF MOVES AT 14
        </div>
      </header>

      <div className="grid lg:grid-cols-[minmax(0,1fr)_19rem]">
        <div className="min-w-0 border-b-2 border-[#17231e] lg:border-r-2 lg:border-b-0">
          <div className="overflow-x-auto p-4 sm:p-6">
            <div className="min-w-[47rem]">
              <div className="mb-2 flex gap-1">
                {PHASES.map((phase) => (
                  <div
                    key={phase.name}
                    style={{ width: phase.width, backgroundColor: phase.color }}
                    className="flex items-center justify-between px-2 py-2 font-mono text-[10px] font-black tracking-[0.12em] text-white uppercase"
                  >
                    <span>{phase.name}</span>
                    <span className="opacity-70">{phase.range}</span>
                  </div>
                ))}
              </div>

              <div className="grid grid-cols-[repeat(14,minmax(46px,1fr))] border-t-2 border-l-2 border-[#17231e] bg-[#fbfcf9]">
                {PIPELINE_STAGES.map((stage) => {
                  const selected = stage.number === active.number
                  const complete = stage.number < active.number
                  const stageColor = PHASE_COLOR[stage.phase]
                  return (
                    <button
                      key={stage.number}
                      type="button"
                      aria-pressed={selected}
                      aria-label={`Stage ${stage.number}: ${stage.title}`}
                      onClick={() => setActiveNumber(stage.number)}
                      className={cn(
                        "group relative flex h-28 flex-col justify-between border-r-2 border-b-2 border-[#17231e] p-2 text-left transition-[background-color,color,transform] duration-150 outline-none focus-visible:z-10 focus-visible:ring-4 focus-visible:ring-[#17231e]/25",
                        selected
                          ? "-translate-y-1 text-white"
                          : "bg-[#fbfcf9] text-[#17231e] hover:bg-white"
                      )}
                      style={
                        selected ? { backgroundColor: stageColor } : undefined
                      }
                    >
                      <span className="font-mono text-xs font-black">
                        {String(stage.number).padStart(2, "0")}
                      </span>
                      <span className="text-[9px] leading-tight font-bold tracking-[0.04em] uppercase [writing-mode:vertical-rl] sm:text-[10px]">
                        {stage.title}
                      </span>
                      <span
                        className={cn(
                          "size-2 border border-[#17231e]",
                          complete && "bg-[#17231e]",
                          selected && "border-white bg-white"
                        )}
                        aria-hidden="true"
                      />
                    </button>
                  )
                })}
              </div>

              <div className="mt-4 flex items-center gap-3">
                <span className="font-mono text-[10px] font-bold text-[#52615a]">
                  START
                </span>
                <div className="h-2 flex-1 border border-[#17231e] bg-[#dce8e3]">
                  <div
                    className="h-full bg-[#17231e] transition-[width] duration-300 motion-reduce:transition-none"
                    style={{ width: `${(active.number / 14) * 100}%` }}
                  />
                </div>
                <span className="font-mono text-[10px] font-bold text-[#52615a]">
                  VISIBLE
                </span>
              </div>
            </div>
          </div>

          <div className="px-4 pb-4 sm:px-6 sm:pb-6" aria-live="polite">
            <div className="grid border-2 border-[#17231e] bg-[#fbfcf9] sm:grid-cols-[1.15fr_1fr]">
              <div className="border-r-2 border-[#17231e] p-4">
                <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#6d7d75]">
                  KEY DATA STRUCTURE
                </p>
                <code className="mt-2 block text-xl font-black tracking-tight text-[#17231e]">
                  {active.structure}
                </code>
                <div className="mt-3 grid grid-cols-2 gap-1.5">
                  {active.fields.map((field, index) => (
                    <div
                      key={field}
                      className="flex min-w-0 items-center gap-2 border border-[#97aaa1] bg-[#edf4f1] px-2 py-2"
                    >
                      <span
                        className="size-2 shrink-0"
                        style={{ backgroundColor: color }}
                        aria-hidden="true"
                      />
                      <code className="min-w-0 text-[9px] font-bold break-all text-[#52615a]">
                        {field}
                      </code>
                      <span className="ml-auto font-mono text-[8px] text-[#97aaa1]">
                        {String(index + 1).padStart(2, "0")}
                      </span>
                    </div>
                  ))}
                </div>
              </div>

              <div className="p-4">
                <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#6d7d75]">
                  STATE CHANGE
                </p>
                <div className="mt-3 grid grid-cols-[minmax(0,1fr)_1.5rem_minmax(0,1fr)] items-stretch">
                  <div className="flex min-h-20 flex-col justify-between border border-[#97aaa1] bg-white p-2.5">
                    <span className="font-mono text-[8px] font-black tracking-[0.12em] text-[#97aaa1]">
                      BEFORE
                    </span>
                    <span className="font-mono text-[10px] leading-4 font-bold text-[#52615a]">
                      {active.before}
                    </span>
                  </div>
                  <div
                    className="flex items-center justify-center text-base font-black text-[#17231e]"
                    aria-hidden="true"
                  >
                    →
                  </div>
                  <div
                    className="flex min-h-20 flex-col justify-between border-2 p-2.5"
                    style={{
                      borderColor: color,
                      backgroundColor: `${color}18`,
                    }}
                  >
                    <span
                      className="font-mono text-[8px] font-black tracking-[0.12em]"
                      style={{ color }}
                    >
                      AFTER
                    </span>
                    <span className="font-mono text-[10px] leading-4 font-black text-[#17231e]">
                      {active.after}
                    </span>
                  </div>
                </div>
              </div>
            </div>
          </div>
        </div>

        <div className="flex flex-col bg-[#fbfcf9]">
          <div className="flex flex-1 flex-col p-5 sm:p-6" aria-live="polite">
            <div className="flex items-start justify-between gap-4">
              <span
                className="font-mono text-5xl leading-none font-black"
                style={{ color }}
              >
                {String(active.number).padStart(2, "0")}
              </span>
              <span
                className="px-2 py-1 font-mono text-[10px] font-black tracking-[0.14em] text-white uppercase"
                style={{ backgroundColor: color }}
              >
                {active.phase}
              </span>
            </div>
            <h4 className="m-0 mt-5 text-xl font-black tracking-tight text-[#17231e]">
              {active.title}
            </h4>
            <p className="m-0 mt-2 text-sm leading-6 text-[#52615a]">
              {active.action}
            </p>

            <div className="mt-6 border-t border-dashed border-[#97aaa1] pt-4">
              <p className="m-0 font-mono text-[9px] font-bold tracking-[0.16em] text-[#6d7d75]">
                GUARANTEE ADDED
              </p>
              <p className="m-0 mt-1 text-sm font-bold text-[#17231e]">
                {active.evidence}
              </p>
            </div>
          </div>

          <div
            className={cn(
              "m-4 flex items-center justify-between border-2 border-[#17231e] px-4 py-3 transition-colors duration-300 motion-reduce:transition-none sm:m-5",
              published ? "bg-[#dff3d8]" : "bg-[#fff0c7]"
            )}
          >
            <div>
              <p className="m-0 font-mono text-[9px] font-black tracking-[0.15em] text-[#52615a]">
                READERS SEE
              </p>
              <p className="m-0 mt-0.5 text-sm font-black text-[#17231e]">
                main → {published ? "B" : "A"}
              </p>
            </div>
            <div
              className={cn(
                "-rotate-3 border-2 px-2 py-1 font-mono text-xs font-black tracking-wider",
                published
                  ? "border-[#267236] text-[#267236]"
                  : "border-[#9a6416] text-[#9a6416]"
              )}
            >
              {published ? "PUBLISHED" : "UNCHANGED"}
            </div>
          </div>
        </div>
      </div>

      <figcaption className="border-t-2 border-[#17231e] bg-[#17231e] px-4 py-3 text-xs leading-5 text-[#dce8e3] sm:px-6">
        Stages 1–13 prepare evidence. Select stage 14 to cross the visibility
        boundary.
      </figcaption>
    </figure>
  )
}

type LaneFocus = "both" | "git" | "crab"

export function PushDataLanesDiagram() {
  const [focus, setFocus] = useState<LaneFocus>("both")
  const gitActive = focus !== "crab"
  const crabActive = focus !== "git"

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(62rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden border border-[#ccd7df] bg-white min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(62rem,calc(100vw-2rem))] lg:w-[min(62rem,calc(100vw-24.5rem))]">
      <div className="flex flex-wrap items-center justify-between gap-3 border-b border-[#ccd7df] px-4 py-4 sm:px-6">
        <div>
          <p className="m-0 font-mono text-[10px] font-bold tracking-[0.18em] text-[#687987]">
            DUAL-LANE ROUTE MAP
          </p>
          <h3 className="m-0 mt-1 text-lg font-bold tracking-tight text-[#152532]">
            Two object types arrive at one gate
          </h3>
        </div>
        <div
          className="flex border border-[#9eacb7] p-1"
          aria-label="Highlight a push data lane"
        >
          {(["both", "git", "crab"] as const).map((option) => (
            <button
              key={option}
              type="button"
              aria-pressed={focus === option}
              onClick={() => setFocus(option)}
              className={cn(
                "px-3 py-1.5 font-mono text-[10px] font-bold uppercase transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#152532]",
                focus === option
                  ? "bg-[#152532] text-white"
                  : "text-[#687987] hover:bg-[#edf2f5]"
              )}
            >
              {option === "both" ? "Both lanes" : option}
            </button>
          ))}
        </div>
      </div>

      <div className="overflow-x-auto bg-[#f4f7f8] px-4 py-6 sm:px-6">
        <svg
          viewBox="0 0 900 330"
          role="img"
          aria-label="Git objects and Crab chunks travel in parallel to durable storage, pass separate closure proofs, and meet at one ref gate."
          className="min-w-[48rem]"
        >
          <defs>
            <pattern
              id="push-map-grid"
              width="24"
              height="24"
              patternUnits="userSpaceOnUse"
            >
              <path
                d="M24 0H0V24"
                fill="none"
                stroke="#dce4e8"
                strokeWidth="1"
              />
            </pattern>
            <marker
              id="push-arrow-git"
              markerWidth="8"
              markerHeight="8"
              refX="7"
              refY="4"
              orient="auto"
            >
              <path d="M0 0L8 4L0 8Z" fill="#e46b32" />
            </marker>
            <marker
              id="push-arrow-crab"
              markerWidth="8"
              markerHeight="8"
              refX="7"
              refY="4"
              orient="auto"
            >
              <path d="M0 0L8 4L0 8Z" fill="#008f83" />
            </marker>
          </defs>
          <rect width="900" height="330" fill="url(#push-map-grid)" />

          <g
            opacity={gitActive ? 1 : 0.18}
            className="transition-opacity duration-200 motion-reduce:transition-none"
          >
            <path
              d="M118 105H700"
              fill="none"
              stroke="#e46b32"
              strokeWidth="10"
              markerEnd="url(#push-arrow-git)"
            />
            <text
              x="30"
              y="76"
              fill="#a64219"
              fontSize="12"
              fontWeight="800"
              letterSpacing="1.2"
            >
              GIT LANE
            </text>
            <RailStop
              x={118}
              y={105}
              color="#e46b32"
              number="06"
              title="Build pack"
            />
            <RailStop
              x={338}
              y={105}
              color="#e46b32"
              number="09"
              title="Upload pack"
            />
            <RailStop
              x={560}
              y={105}
              color="#e46b32"
              number="11"
              title="Prove Git"
            />
          </g>

          <g
            opacity={crabActive ? 1 : 0.18}
            className="transition-opacity duration-200 motion-reduce:transition-none"
          >
            <path
              d="M118 230H700"
              fill="none"
              stroke="#008f83"
              strokeWidth="10"
              markerEnd="url(#push-arrow-crab)"
            />
            <text
              x="30"
              y="201"
              fill="#00675f"
              fontSize="12"
              fontWeight="800"
              letterSpacing="1.2"
            >
              CRAB LANE
            </text>
            <RailStop
              x={118}
              y={230}
              color="#008f83"
              number="07"
              title="Build xorbs"
            />
            <RailStop
              x={338}
              y={230}
              color="#008f83"
              number="08–10"
              title="Data + shards"
            />
            <RailStop
              x={560}
              y={230}
              color="#008f83"
              number="12"
              title="Prove pointers"
            />
          </g>

          <path
            d="M704 105C755 105 750 165 780 165M704 230C755 230 750 165 780 165"
            fill="none"
            stroke="#152532"
            strokeWidth="3"
            strokeDasharray="6 5"
          />
          <rect x="778" y="117" width="98" height="96" fill="#152532" />
          <text
            x="827"
            y="145"
            fill="#a9bac5"
            fontSize="10"
            fontWeight="700"
            textAnchor="middle"
            letterSpacing="1"
          >
            REF GATE
          </text>
          <text
            x="827"
            y="176"
            fill="white"
            fontSize="24"
            fontWeight="900"
            textAnchor="middle"
          >
            13 → 14
          </text>
          <text
            x="827"
            y="197"
            fill="#a9bac5"
            fontSize="10"
            textAnchor="middle"
          >
            COMPARE · COMMIT
          </text>
        </svg>
      </div>

      <figcaption className="flex items-start gap-2 border-t border-[#ccd7df] px-4 py-3 text-xs leading-5 text-[#526674] sm:px-6">
        <PackageCheck
          size={15}
          className="mt-0.5 shrink-0 text-[#008f83]"
          aria-hidden="true"
        />
        The lanes upload independently, but neither can publish alone. Both
        proofs must reach the ref gate.
      </figcaption>
    </figure>
  )
}

function RailStop({
  x,
  y,
  color,
  number,
  title,
}: {
  x: number
  y: number
  color: string
  number: string
  title: string
}) {
  return (
    <g>
      <circle
        cx={x}
        cy={y}
        r="18"
        fill="white"
        stroke={color}
        strokeWidth="6"
      />
      <text
        x={x}
        y={y + 4}
        textAnchor="middle"
        fill="#152532"
        fontSize="10"
        fontWeight="900"
      >
        {number}
      </text>
      <text
        x={x}
        y={y + 38}
        textAnchor="middle"
        fill="#526674"
        fontSize="11"
        fontWeight="700"
      >
        {title}
      </text>
    </g>
  )
}

type VisibilityCase = {
  label: string
  title: string
  visible: "A" | "B" | "C"
  status: "unchanged" | "published" | "winner"
  durable: string
  next: string
  summary: string
  icon: typeof Cloud
}

const VISIBILITY_CASES: VisibilityCase[] = [
  {
    label: "Upload stops",
    title: "Object upload is interrupted",
    visible: "A",
    status: "unchanged",
    durable: "Some complete immutable objects",
    next: "Retry and reuse completed uploads",
    summary: "No closure proof, so the ref gate stays closed.",
    icon: Cloud,
  },
  {
    label: "Proof fails",
    title: "A dependency cannot be proven",
    visible: "A",
    status: "unchanged",
    durable: "An incomplete dependency set",
    next: "Restore the missing object or metadata",
    summary: "Canonical storage—not the local cache—must prove closure.",
    icon: ShieldCheck,
  },
  {
    label: "Alice wins",
    title: "Expected-old A still matches",
    visible: "B",
    status: "published",
    durable: "Complete Git and Crab closures",
    next: "Release the lock and report success",
    summary: "Stage 14 commits the ref transaction A → B.",
    icon: Check,
  },
  {
    label: "Bob is stale",
    title: "Another writer already moved main",
    visible: "C",
    status: "winner",
    durable: "Bob's uploaded objects remain safe",
    next: "Fetch C, reconcile, and retry",
    summary: "Expected-old rejects Bob instead of overwriting newer history.",
    icon: GitBranch,
  },
  {
    label: "Cleanup fails",
    title: "The ref committed before cleanup failed",
    visible: "B",
    status: "published",
    durable: "The complete published state",
    next: "Finish cleanup; do not roll back",
    summary: "Post-commit repair moves forward from valid durable state.",
    icon: AlertTriangle,
  },
]

export function PushVisibilityLab() {
  const [activeIndex, setActiveIndex] = useState(0)
  const active = VISIBILITY_CASES[activeIndex]
  const Icon = active.icon
  const changed = active.visible !== "A"

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(62rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden bg-[#f3c94f] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(62rem,calc(100vw-2rem))] lg:w-[min(62rem,calc(100vw-24.5rem))]">
      <div className="grid lg:grid-cols-[16rem_minmax(0,1fr)]">
        <div className="border-b-2 border-[#191d1b] bg-[#191d1b] p-4 text-white sm:p-5 lg:border-r-2 lg:border-b-0">
          <p className="m-0 font-mono text-[10px] font-bold tracking-[0.18em] text-[#f3c94f]">
            INCIDENT SELECTOR
          </p>
          <h3 className="m-0 mt-1 text-xl font-black tracking-tight">
            Where did the push stop?
          </h3>
          <div className="mt-5 grid gap-1.5 sm:grid-cols-2 lg:grid-cols-1">
            {VISIBILITY_CASES.map((item, index) => (
              <button
                key={item.label}
                type="button"
                aria-pressed={index === activeIndex}
                onClick={() => setActiveIndex(index)}
                className={cn(
                  "flex items-center justify-between gap-3 border px-3 py-2.5 text-left text-xs font-bold transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#f3c94f]",
                  index === activeIndex
                    ? "border-[#f3c94f] bg-[#f3c94f] text-[#191d1b]"
                    : "border-[#4b504d] text-[#d7dcd9] hover:border-[#f3c94f]"
                )}
              >
                {item.label}
                <ArrowRight size={14} aria-hidden="true" />
              </button>
            ))}
          </div>
        </div>

        <div className="p-4 sm:p-6 lg:p-8" aria-live="polite">
          <div className="flex flex-wrap items-start justify-between gap-5 border-b-2 border-[#191d1b] pb-6">
            <div className="max-w-lg">
              <div className="flex size-10 items-center justify-center border-2 border-[#191d1b] bg-white">
                <Icon size={19} aria-hidden="true" />
              </div>
              <h4 className="m-0 mt-4 text-2xl leading-tight font-black tracking-[-0.035em] text-[#191d1b] sm:text-3xl">
                {active.title}
              </h4>
              <p className="m-0 mt-2 text-sm leading-6 text-[#4b4123]">
                {active.summary}
              </p>
            </div>

            <div className="min-w-40 border-2 border-[#191d1b] bg-white px-4 py-3 text-center shadow-[4px_4px_0_#191d1b]">
              <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#706744]">
                READERS SEE
              </p>
              <p className="m-0 mt-1 text-5xl leading-none font-black text-[#191d1b]">
                {active.visible}
              </p>
              <p className="m-0 mt-1 font-mono text-[9px] font-bold tracking-[0.12em] text-[#706744] uppercase">
                {active.status}
              </p>
            </div>
          </div>

          <div className="grid gap-4 py-6 sm:grid-cols-2">
            <div className="border-l-4 border-[#191d1b] pl-3">
              <p className="m-0 font-mono text-[9px] font-black tracking-[0.15em] text-[#706744]">
                MAY BE DURABLE
              </p>
              <p className="m-0 mt-1 text-sm font-bold text-[#191d1b]">
                {active.durable}
              </p>
            </div>
            <div className="border-l-4 border-[#191d1b] pl-3">
              <p className="m-0 font-mono text-[9px] font-black tracking-[0.15em] text-[#706744]">
                NEXT MOVE
              </p>
              <p className="m-0 mt-1 text-sm font-bold text-[#191d1b]">
                {active.next}
              </p>
            </div>
          </div>

          <div className="flex items-center gap-2 border-t-2 border-[#191d1b] pt-4 font-mono text-[10px] font-black tracking-[0.08em] text-[#191d1b]">
            <span
              className={cn(
                "size-3 border-2 border-[#191d1b]",
                changed ? "bg-[#e24e3f]" : "bg-white"
              )}
              aria-hidden="true"
            />
            REF GATE {changed ? "COMMITTED OR WON ELSEWHERE" : "DID NOT COMMIT"}
          </div>
        </div>
      </div>
      <figcaption className="border-t-2 border-[#191d1b] bg-[#fff4c8] px-4 py-3 text-xs leading-5 text-[#4b4123] sm:px-6">
        Change the incident to see the only state readers can observe at each
        boundary.
      </figcaption>
    </figure>
  )
}
