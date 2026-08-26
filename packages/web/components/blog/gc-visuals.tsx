"use client"

import {
  ArrowRight,
  GitBranch,
  History,
  LockKeyhole,
  ShieldCheck,
  Trash2,
  X,
} from "lucide-react"
import { useState } from "react"

import { cn } from "@/lib/utils"

type ObjectCase = {
  id: "main" | "history" | "recent" | "candidate" | "shared"
  label: string
  object: string
  size: string
  age: string
  path: string
  roots: { label: string; kind: "live" | "retained" | "none" }[]
  unreachable: boolean
  oldEnough: boolean
  verdict: "RETAIN" | "PROTECTED" | "CANDIDATE"
  reason: string
  structures: string[]
}

const OBJECT_CASES: ObjectCase[] = [
  {
    id: "main",
    label: "On main",
    object: "xorb 19e2…",
    size: "12 GB",
    age: "90 days",
    path: "models/current/model.safetensors",
    roots: [
      { label: "refs/heads/main", kind: "live" },
      { label: "pointer → recipe", kind: "live" },
      { label: "recipe → xorb 19e2", kind: "live" },
    ],
    unreachable: false,
    oldEnough: true,
    verdict: "RETAIN",
    reason: "Age cannot override a live path from main.",
    structures: ["root set", "pointer map", "mark set"],
  },
  {
    id: "history",
    label: "Deleted branch",
    object: "xorb a840…",
    size: "6.4 GB",
    age: "41 days",
    path: "experiments/branch-17/output.bin",
    roots: [
      { label: "feature branch deleted", kind: "none" },
      { label: "recovery generation 184", kind: "retained" },
      { label: "recipe → xorb a840", kind: "retained" },
    ],
    unreachable: false,
    oldEnough: true,
    verdict: "RETAIN",
    reason: "Recovery history still reaches the object.",
    structures: ["history root", "generation", "mark set"],
  },
  {
    id: "recent",
    label: "Interrupted push",
    object: "xorb 6cb1…",
    size: "800 MB",
    age: "18 minutes",
    path: "staging/push-7/xorb-6cb1",
    roots: [{ label: "no published root", kind: "none" }],
    unreachable: true,
    oldEnough: false,
    verdict: "PROTECTED",
    reason: "It is unreachable, but still inside the 24-hour grace window.",
    structures: ["object metadata", "snapshot time", "grace cutoff"],
  },
  {
    id: "candidate",
    label: "Old orphan",
    object: "xorb d77c…",
    size: "4.2 GB",
    age: "3 days",
    path: "xorbs/d7/d77c…",
    roots: [{ label: "no retained root", kind: "none" }],
    unreachable: true,
    oldEnough: true,
    verdict: "CANDIDATE",
    reason: "Both deletion proofs pass. Dry run may list this object.",
    structures: ["unmarked listing", "candidate batch", "ETag + size"],
  },
  {
    id: "shared",
    label: "Shared xorb",
    object: "xorb f093…",
    size: "9.8 GB",
    age: "120 days",
    path: "xorbs/f0/f093…",
    roots: [
      { label: "repo/vision: unreferenced", kind: "none" },
      { label: "repo/labs: recipe → xorb", kind: "live" },
    ],
    unreachable: false,
    oldEnough: true,
    verdict: "RETAIN",
    reason:
      "Bucket scope keeps an object reached by any registered repository.",
    structures: ["repository registry", "union of marks", "shared xorb"],
  },
]

const VERDICT_STYLE = {
  RETAIN: "border-[#2f7d63] bg-[#dcece6] text-[#205743]",
  PROTECTED: "border-[#d99a27] bg-[#fff0c9] text-[#714d12]",
  CANDIDATE: "border-[#c64e44] bg-[#f7d9d5] text-[#7a2923]",
}

export function GcObjectEvidenceLab() {
  const [caseId, setCaseId] = useState<ObjectCase["id"]>("main")
  const selected =
    OBJECT_CASES.find((item) => item.id === caseId) ?? OBJECT_CASES[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.75rem] border border-[#617785] bg-[#eef4f3] text-[#20313a] shadow-[0_20px_60px_rgba(19,42,58,0.16)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="border-b border-[#aebdc1] bg-[#132a3a] px-5 py-5 text-white sm:px-7">
        <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-[#f6c85f]">
          OBJECT EVIDENCE LAB / SELECT A SPECIMEN
        </p>
        <div className="mt-3 flex flex-wrap gap-2" aria-label="Object example">
          {OBJECT_CASES.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-pressed={selected.id === item.id}
              onClick={() => setCaseId(item.id)}
              className={cn(
                "min-h-11 rounded-full border px-3 py-2 font-mono text-[10px] font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#f6c85f] focus-visible:ring-offset-2 focus-visible:ring-offset-[#132a3a]",
                selected.id === item.id
                  ? "border-[#f6c85f] bg-[#f6c85f] text-[#132a3a]"
                  : "border-[#617785] text-[#c6d5d9] hover:border-[#f6c85f] hover:text-white"
              )}
            >
              {item.label}
            </button>
          ))}
        </div>
      </header>

      <div className="grid lg:grid-cols-[1.1fr_0.9fr]" aria-live="polite">
        <section className="border-b border-[#aebdc1] p-5 sm:p-7 lg:border-r lg:border-b-0">
          <div className="grid gap-4 sm:grid-cols-[1fr_auto] sm:items-start">
            <div>
              <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#667a83]">
                STORAGE OBJECT
              </p>
              <h3 className="m-0 mt-1 font-mono text-3xl font-black tracking-[-0.06em] text-[#2e6f95]">
                {selected.object}
              </h3>
              <p className="m-0 mt-2 font-mono text-[10px] break-all text-[#667a83]">
                {selected.path}
              </p>
            </div>
            <div className="grid grid-cols-2 gap-2 text-right font-mono text-[10px]">
              <div className="rounded-xl border border-[#aebdc1] bg-white p-3">
                <span className="block text-[#667a83]">SIZE</span>
                <strong className="mt-1 block text-sm">{selected.size}</strong>
              </div>
              <div className="rounded-xl border border-[#aebdc1] bg-white p-3">
                <span className="block text-[#667a83]">AGE</span>
                <strong className="mt-1 block text-sm">{selected.age}</strong>
              </div>
            </div>
          </div>

          <div className="mt-6 rounded-2xl border border-[#8fa4aa] bg-white p-4">
            <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#667a83]">
              ROOT TRACE
            </p>
            <div className="mt-4 grid gap-2">
              {selected.roots.map((root, index) => (
                <div key={root.label} className="contents">
                  <div
                    className={cn(
                      "flex min-h-12 items-center gap-3 rounded-xl border px-4 py-3 font-mono text-[10px] font-bold",
                      root.kind === "live" &&
                        "border-[#6ca58e] bg-[#e6f2ed] text-[#205743]",
                      root.kind === "retained" &&
                        "border-[#759cb5] bg-[#e6eff4] text-[#285b79]",
                      root.kind === "none" &&
                        "border-dashed border-[#aebdc1] bg-[#f4f6f5] text-[#77868b]"
                    )}
                  >
                    {root.kind === "none" ? (
                      <X className="size-4 shrink-0" aria-hidden="true" />
                    ) : root.kind === "retained" ? (
                      <History className="size-4 shrink-0" aria-hidden="true" />
                    ) : (
                      <GitBranch
                        className="size-4 shrink-0"
                        aria-hidden="true"
                      />
                    )}
                    {root.label}
                  </div>
                  {index < selected.roots.length - 1 ? (
                    <ArrowRight
                      className="mx-auto size-4 rotate-90 text-[#8fa4aa]"
                      aria-hidden="true"
                    />
                  ) : null}
                </div>
              ))}
            </div>
          </div>
        </section>

        <section className="bg-white p-5 sm:p-7">
          <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#667a83]">
            TWO-KEY DELETION PERMIT
          </p>
          <div className="mt-4 grid gap-3">
            <ProofKey
              label="Unreachable from every retained root"
              passes={selected.unreachable}
              passLabel="CLEAR"
              failLabel="BLOCKED"
            />
            <ProofKey
              label="Older than the 24-hour grace window"
              passes={selected.oldEnough}
              passLabel="CLEAR"
              failLabel="BLOCKED"
            />
          </div>

          <div
            className={cn(
              "mt-5 rounded-2xl border-2 p-5",
              VERDICT_STYLE[selected.verdict]
            )}
          >
            <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] opacity-75">
              CLASSIFICATION
            </p>
            <p className="m-0 mt-1 font-mono text-4xl font-black tracking-[-0.06em]">
              {selected.verdict}
            </p>
            <p className="m-0 mt-3 text-sm leading-6">{selected.reason}</p>
          </div>

          <div className="mt-5 border-t border-dashed border-[#aebdc1] pt-4">
            <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#667a83]">
              EVIDENCE STRUCTURES
            </p>
            <div className="mt-3 flex flex-wrap gap-2">
              {selected.structures.map((structure) => (
                <span
                  key={structure}
                  className="rounded-full border border-[#aebdc1] bg-[#eef4f3] px-3 py-1.5 font-mono text-[10px] text-[#4e626b]"
                >
                  {structure}
                </span>
              ))}
            </div>
          </div>
        </section>
      </div>
    </figure>
  )
}

function ProofKey({
  label,
  passes,
  passLabel,
  failLabel,
}: {
  label: string
  passes: boolean
  passLabel: string
  failLabel: string
}) {
  return (
    <div className="grid grid-cols-[auto_1fr_auto] items-center gap-3 rounded-xl border border-[#aebdc1] bg-[#eef4f3] p-3">
      <LockKeyhole
        className={cn("size-5", passes ? "text-[#2f7d63]" : "text-[#c64e44]")}
        aria-hidden="true"
      />
      <p className="m-0 text-xs leading-5 font-bold">{label}</p>
      <span
        className={cn(
          "rounded-full px-2 py-1 font-mono text-[9px] font-black",
          passes ? "bg-[#dcece6] text-[#205743]" : "bg-[#f7d9d5] text-[#7a2923]"
        )}
      >
        {passes ? passLabel : failLabel}
      </span>
    </div>
  )
}

type RaceCase = {
  id: "publishes" | "abandoned" | "crossing"
  label: string
  title: string
  result: string
  resultTone: "keep" | "delete" | "stop"
  summary: string
  stateBefore: string
  stateAfter: string
  stages: {
    time: string
    label: string
    detail: string
    tone: "gc" | "writer" | "safe" | "stop"
  }[]
}

const RACE_CASES: RaceCase[] = [
  {
    id: "publishes",
    label: "Push publishes",
    title: "A fresh upload looks orphaned only briefly",
    result: "KEEP",
    resultTone: "keep",
    summary:
      "Grace protects the upload now; the published ref marks it live on the next run.",
    stateBefore: "roots: none",
    stateAfter: "roots: main → xorb",
    stages: [
      {
        time: "T0",
        label: "GC snapshots roots",
        detail: "xorb not present",
        tone: "gc",
      },
      {
        time: "+2m",
        label: "Writer uploads xorb",
        detail: "not published yet",
        tone: "writer",
      },
      {
        time: "+8m",
        label: "GC sees object",
        detail: "inside 24h grace",
        tone: "safe",
      },
      {
        time: "+11m",
        label: "Writer publishes ref",
        detail: "reachable next run",
        tone: "writer",
      },
    ],
  },
  {
    id: "abandoned",
    label: "Push abandoned",
    title: "An abandoned upload ages into a candidate",
    result: "CANDIDATE",
    resultTone: "delete",
    summary:
      "No retained root appears. After 24 hours, both deletion proofs can pass.",
    stateBefore: "age: 18 min",
    stateAfter: "age: 30 hours",
    stages: [
      {
        time: "T-30h",
        label: "Writer uploads xorb",
        detail: "push interrupted",
        tone: "writer",
      },
      {
        time: "T0",
        label: "GC snapshots roots",
        detail: "no publication",
        tone: "gc",
      },
      {
        time: "+4m",
        label: "Mark walk ends",
        detail: "xorb unmarked",
        tone: "gc",
      },
      {
        time: "+6m",
        label: "Age check passes",
        detail: "older than 24h",
        tone: "stop",
      },
    ],
  },
  {
    id: "crossing",
    label: "Writer crosses batches",
    title: "A new writer invalidates the sealed sweep",
    result: "STOP",
    resultTone: "stop",
    summary:
      "The writer epoch changes. GC fails closed before deleting another batch.",
    stateBefore: "writer_epoch 42",
    stateAfter: "writer_epoch 43",
    stages: [
      {
        time: "T0",
        label: "GC seals root snapshot",
        detail: "epoch 42",
        tone: "gc",
      },
      {
        time: "+5m",
        label: "Batch 1 completes",
        detail: "512 candidates",
        tone: "gc",
      },
      {
        time: "+6m",
        label: "Writer admitted",
        detail: "epoch advances",
        tone: "writer",
      },
      {
        time: "+7m",
        label: "Next fence check",
        detail: "epoch mismatch",
        tone: "stop",
      },
    ],
  },
]

const RACE_TONE = {
  gc: "border-[#7896a5] bg-[#1d3a4b]",
  writer: "border-[#f6c85f] bg-[#4a432d]",
  safe: "border-[#68a58d] bg-[#21483e]",
  stop: "border-[#d17269] bg-[#4d2e31]",
}

export function GcRaceReplay() {
  const [caseId, setCaseId] = useState<RaceCase["id"]>("publishes")
  const selected =
    RACE_CASES.find((item) => item.id === caseId) ?? RACE_CASES[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.75rem] bg-[#132a3a] text-white shadow-[0_20px_60px_rgba(19,42,58,0.22)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="grid gap-5 border-b border-[#526c79] px-5 py-5 sm:px-7 lg:grid-cols-[1fr_auto] lg:items-end">
        <div>
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-[#f6c85f]">
            CONCURRENCY REPLAY / MOVE THE WRITER
          </p>
          <h3 className="m-0 mt-2 text-2xl font-black tracking-[-0.04em] sm:text-3xl">
            What happens around the snapshot?
          </h3>
        </div>
        <div className="flex flex-wrap gap-2" aria-label="Concurrency example">
          {RACE_CASES.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-pressed={selected.id === item.id}
              onClick={() => setCaseId(item.id)}
              className={cn(
                "min-h-11 rounded-lg border px-3 py-2 font-mono text-[10px] font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#f6c85f] focus-visible:ring-offset-2 focus-visible:ring-offset-[#132a3a]",
                selected.id === item.id
                  ? "border-[#f6c85f] bg-[#f6c85f] text-[#132a3a]"
                  : "border-[#617785] text-[#c6d5d9] hover:border-[#f6c85f] hover:text-white"
              )}
            >
              {item.label}
            </button>
          ))}
        </div>
      </header>

      <div className="p-5 sm:p-7" aria-live="polite">
        <div className="grid gap-4 lg:grid-cols-[1fr_auto] lg:items-start">
          <div>
            <h4 className="m-0 text-xl font-black sm:text-2xl">
              {selected.title}
            </h4>
            <p className="m-0 mt-2 max-w-2xl text-sm leading-6 text-[#c6d5d9]">
              {selected.summary}
            </p>
          </div>
          <div className="grid grid-cols-[1fr_auto_1fr] items-center gap-3 rounded-xl border border-[#526c79] bg-[#1d3a4b] px-4 py-3 font-mono text-[10px] font-black">
            <span className="text-[#a9bdc5]">{selected.stateBefore}</span>
            <ArrowRight className="size-4 text-[#f6c85f]" aria-hidden="true" />
            <span className="text-right">{selected.stateAfter}</span>
          </div>
        </div>

        <div className="mt-7 grid gap-2 lg:grid-cols-[1fr_auto_1fr_auto_1fr_auto_1fr] lg:items-stretch">
          {selected.stages.map((stage, index) => (
            <div key={`${selected.id}-${stage.time}`} className="contents">
              <div
                className={cn(
                  "min-h-36 rounded-2xl border p-4",
                  RACE_TONE[stage.tone]
                )}
              >
                <p className="m-0 font-mono text-[10px] font-black text-[#f6c85f]">
                  {stage.time}
                </p>
                <p className="m-0 mt-7 text-sm font-black">{stage.label}</p>
                <p className="m-0 mt-1 font-mono text-[9px] leading-4 text-[#c6d5d9]">
                  {stage.detail}
                </p>
              </div>
              {index < selected.stages.length - 1 ? (
                <div className="flex items-center justify-center text-[#7896a5]">
                  <ArrowRight
                    className="size-4 rotate-90 lg:rotate-0"
                    aria-hidden="true"
                  />
                </div>
              ) : null}
            </div>
          ))}
        </div>

        <div className="mt-6 flex flex-wrap items-center justify-between gap-4 border-t border-dashed border-[#617785] pt-5">
          <div className="flex items-center gap-3">
            {selected.resultTone === "keep" ? (
              <ShieldCheck
                className="size-6 text-[#68a58d]"
                aria-hidden="true"
              />
            ) : selected.resultTone === "delete" ? (
              <Trash2 className="size-6 text-[#d17269]" aria-hidden="true" />
            ) : (
              <LockKeyhole
                className="size-6 text-[#f6c85f]"
                aria-hidden="true"
              />
            )}
            <span className="font-mono text-2xl font-black tracking-[-0.05em]">
              {selected.result}
            </span>
          </div>
          <p className="m-0 font-mono text-[9px] font-bold text-[#91a8b2]">
            STATE: root set · object age · writer epoch
          </p>
        </div>
      </div>
    </figure>
  )
}
