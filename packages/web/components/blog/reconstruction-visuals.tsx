"use client"

import {
  ArrowDown,
  ArrowRight,
  Check,
  CircleX,
  FileCheck2,
  FileCode2,
  Gauge,
  PackageOpen,
  RotateCcw,
  ShieldCheck,
  Wrench,
} from "lucide-react"
import { useState } from "react"

import { cn } from "@/lib/utils"

type RecipeTerm = {
  position: number
  chunk: string
  xorb: string
  range: string
  bytes: string
  source: "origin" | "cache" | "reused read"
  color: string
}

const RECIPE: RecipeTerm[] = [
  {
    position: 1,
    chunk: "A",
    xorb: "xorb-17",
    range: "chunks 0..1",
    bytes: "72 KiB",
    source: "origin",
    color: "#77B6D1",
  },
  {
    position: 2,
    chunk: "B",
    xorb: "xorb-42",
    range: "chunks 3..4",
    bytes: "64 KiB",
    source: "origin",
    color: "#F6C85F",
  },
  {
    position: 3,
    chunk: "C",
    xorb: "xorb-17",
    range: "chunks 1..2",
    bytes: "81 KiB",
    source: "origin",
    color: "#A9D18E",
  },
  {
    position: 4,
    chunk: "D",
    xorb: "xorb-99",
    range: "chunks 8..9",
    bytes: "59 KiB",
    source: "cache",
    color: "#D6A5CF",
  },
  {
    position: 5,
    chunk: "B",
    xorb: "xorb-42",
    range: "chunks 3..4",
    bytes: "64 KiB",
    source: "reused read",
    color: "#F6C85F",
  },
]

export function ReconstructionWorkbench() {
  const [selectedPosition, setSelectedPosition] = useState(1)
  const selected =
    RECIPE.find((term) => term.position === selectedPosition) ?? RECIPE[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-xl border-4 border-[#203948] bg-[#d8e1e4] shadow-[0_18px_50px_rgba(32,57,72,0.18)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="flex flex-wrap items-end justify-between gap-5 border-b-4 border-[#203948] bg-[#234e70] px-5 py-5 text-white sm:px-7">
        <div>
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-[#b9d7e5]">
            FILE RECONSTRUCTION BENCH / TOY RECIPE
          </p>
          <h3 className="m-0 mt-1 text-2xl font-black tracking-[-0.03em] sm:text-3xl">
            One stored chunk can fill two file positions.
          </h3>
        </div>
        <div className="flex items-center gap-2 rounded-md border border-[#86a9bc] bg-[#17384f] px-3 py-2 font-mono text-[10px] font-bold text-[#d9edf5]">
          <Wrench className="size-4" aria-hidden="true" />
          SELECT AN OUTPUT SLOT
        </div>
      </header>

      <div className="grid lg:grid-cols-[minmax(0,1fr)_17rem]">
        <div className="p-4 sm:p-6">
          <div className="rounded-lg border-2 border-[#203948] bg-[#f7f8f4] p-4 shadow-[inset_0_-8px_0_#e9ece8] sm:p-5">
            <div className="flex items-center justify-between gap-4">
              <p className="m-0 font-mono text-[9px] font-black tracking-[0.17em] text-[#66777d]">
                LOGICAL FILE RECIPE
              </p>
              <span className="font-mono text-[9px] font-bold text-[#66777d]">
                WRITE ORDER →
              </span>
            </div>
            <div className="mt-3 grid grid-cols-5 gap-2">
              {RECIPE.map((term) => (
                <button
                  key={term.position}
                  type="button"
                  aria-pressed={selected.position === term.position}
                  aria-label={`Select file position ${term.position}, chunk ${term.chunk}`}
                  onClick={() => setSelectedPosition(term.position)}
                  className={cn(
                    "group relative flex h-24 flex-col items-center justify-center rounded-md border-2 font-mono transition-transform outline-none hover:-translate-y-1 focus-visible:ring-3 focus-visible:ring-[#234e70] motion-reduce:transition-none",
                    selected.position === term.position
                      ? "-translate-y-1 border-[#203948] shadow-[0_5px_0_#203948]"
                      : "border-[#87979d]"
                  )}
                  style={{ backgroundColor: term.color }}
                >
                  <span className="absolute top-1.5 left-2 text-[8px] font-black text-[#253840]">
                    {String(term.position).padStart(2, "0")}
                  </span>
                  <span className="text-3xl font-black text-[#172a34]">
                    {term.chunk}
                  </span>
                  {term.position === 5 ? (
                    <span className="mt-1 rounded-full bg-[#6c5012] px-2 py-0.5 text-[7px] font-black text-white">
                      REPEAT
                    </span>
                  ) : null}
                </button>
              ))}
            </div>
          </div>

          <div className="my-4 flex items-center gap-3 text-[#5f737b]">
            <div className="h-px flex-1 bg-[#8fa0a7]" />
            <ArrowDown className="size-5" aria-hidden="true" />
            <span className="font-mono text-[9px] font-black">
              RESOLVE PLACEMENT
            </span>
            <div className="h-px flex-1 bg-[#8fa0a7]" />
          </div>

          <div className="grid gap-3 sm:grid-cols-3">
            {[
              {
                label: "XORB RACK 17",
                chunks: ["A", "C", "E"],
                color: "bg-[#d7eaf2]",
              },
              {
                label: "XORB RACK 42",
                chunks: ["F", "G", "H", "B"],
                color: "bg-[#f9e8b6]",
              },
              {
                label: "LOCAL CACHE",
                chunks: ["D"],
                color: "bg-[#ead9e7]",
              },
            ].map((rack) => (
              <div
                key={rack.label}
                className="rounded-md border-2 border-[#52666f] bg-[#bec9cc] p-3"
              >
                <p className="m-0 font-mono text-[8px] font-black tracking-[0.14em] text-[#3e5159]">
                  {rack.label}
                </p>
                <div className="mt-3 flex min-h-16 flex-wrap content-start gap-1.5">
                  {rack.chunks.map((chunk) => {
                    const active = selected.chunk === chunk
                    return (
                      <div
                        key={chunk}
                        className={cn(
                          "flex size-10 items-center justify-center rounded border font-mono text-sm font-black transition-all motion-reduce:transition-none",
                          rack.color,
                          active
                            ? "scale-110 border-[#203948] shadow-[0_3px_0_#203948]"
                            : "border-[#82949b] text-[#52666f]"
                        )}
                      >
                        {chunk}
                      </div>
                    )
                  })}
                </div>
              </div>
            ))}
          </div>
        </div>

        <aside
          className="border-t-4 border-[#203948] bg-[#f7f8f4] p-5 lg:border-t-0 lg:border-l-4"
          aria-live="polite"
        >
          <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#6b7c82]">
            SELECTED FILE TERM
          </p>
          <p className="m-0 mt-2 text-5xl font-black tracking-[-0.08em] text-[#234e70]">
            {selected.chunk}
            <span className="ml-2 text-base tracking-normal text-[#6b7c82]">
              / {selected.position}
            </span>
          </p>
          <dl className="m-0 mt-6 grid gap-4">
            {[
              ["PLACEMENT", selected.xorb],
              ["CHUNK RANGE", selected.range],
              ["UNPACKED", selected.bytes],
              ["READ SOURCE", selected.source],
            ].map(([label, value]) => (
              <div key={label} className="border-b border-[#c1cbce] pb-3">
                <dt className="font-mono text-[8px] font-black tracking-[0.14em] text-[#7b898e]">
                  {label}
                </dt>
                <dd className="m-0 mt-1 text-sm font-black text-[#203948]">
                  {value}
                </dd>
              </div>
            ))}
          </dl>
          <p className="m-0 mt-5 text-xs leading-5 text-[#607077]">
            File position {selected.position} keeps its own output slot even
            when its bytes share a physical read.
          </p>
        </aside>
      </div>

      <figcaption className="border-t-4 border-[#203948] bg-[#f6c85f] px-5 py-3 font-mono text-[9px] font-bold text-[#3f330e] sm:px-7">
        Illustrative hashes and sizes. The invariant is real: every recipe
        position must have one valid placement.
      </figcaption>
    </figure>
  )
}

type ArrivalScenario = {
  id: string
  label: string
  arrival: string[]
  note: string
}

const ARRIVAL_SCENARIOS: ArrivalScenario[] = [
  {
    id: "17-first",
    label: "xorb-17 first",
    arrival: ["A", "C", "B", "D"],
    note: "A and C share one object read, but C still waits for output slot 3.",
  },
  {
    id: "42-first",
    label: "xorb-42 first",
    arrival: ["B", "D", "A", "C"],
    note: "B arrives first and fills two logical positions when assembly reaches them.",
  },
  {
    id: "cache-first",
    label: "cache first",
    arrival: ["D", "A", "C", "B"],
    note: "A local hit can finish immediately without moving D ahead of A, B, or C.",
  },
]

const CHUNK_COLORS: Record<string, string> = {
  A: "bg-[#77b6d1]",
  B: "bg-[#f6c85f]",
  C: "bg-[#a9d18e]",
  D: "bg-[#d6a5cf]",
}

export function ReconstructionArrivalBoard() {
  const [scenarioId, setScenarioId] = useState("17-first")
  const scenario =
    ARRIVAL_SCENARIOS.find((item) => item.id === scenarioId) ??
    ARRIVAL_SCENARIOS[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(60rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[2rem] bg-[#17252d] p-3 shadow-[0_18px_55px_rgba(23,37,45,0.2)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(60rem,calc(100vw-2rem))] lg:w-[min(60rem,calc(100vw-24.5rem))]">
      <div className="rounded-[1.25rem] border border-[#57707b] bg-[#20343e] p-5 sm:p-7">
        <header className="flex flex-wrap items-end justify-between gap-5">
          <div>
            <p className="m-0 font-mono text-[10px] font-black tracking-[0.18em] text-[#8db5c4]">
              PARALLEL ARRIVAL BOARD
            </p>
            <h3 className="m-0 mt-1 text-2xl font-black tracking-[-0.03em] text-white">
              Downloads may race. The recipe does not.
            </h3>
          </div>
          <div className="flex flex-wrap gap-2">
            {ARRIVAL_SCENARIOS.map((item) => (
              <button
                key={item.id}
                type="button"
                aria-pressed={scenario.id === item.id}
                onClick={() => setScenarioId(item.id)}
                className={cn(
                  "rounded-full border px-3 py-1.5 font-mono text-[9px] font-black outline-none focus-visible:ring-2 focus-visible:ring-[#f6c85f] focus-visible:ring-offset-2 focus-visible:ring-offset-[#20343e]",
                  scenario.id === item.id
                    ? "border-[#f6c85f] bg-[#f6c85f] text-[#30270c]"
                    : "border-[#66808b] text-[#b9cdd4] hover:border-[#a6c8d5]"
                )}
              >
                {item.label}
              </button>
            ))}
          </div>
        </header>

        <div className="mt-7 grid gap-5 md:grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)] md:items-center">
          <div className="rounded-xl border border-[#57707b] bg-[#162830] p-4">
            <p className="m-0 font-mono text-[9px] font-black tracking-[0.15em] text-[#89a8b3]">
              COMPLETION ORDER
            </p>
            <div className="mt-4 grid grid-cols-4 gap-2">
              {scenario.arrival.map((chunk, index) => (
                <div key={chunk} className="text-center">
                  <div
                    className={cn(
                      "flex h-16 items-center justify-center rounded-md border border-white/30 font-mono text-xl font-black text-[#18252b]",
                      CHUNK_COLORS[chunk]
                    )}
                  >
                    {chunk}
                  </div>
                  <span className="mt-1 block font-mono text-[8px] font-bold text-[#829da8]">
                    ARRIVAL {index + 1}
                  </span>
                </div>
              ))}
            </div>
          </div>

          <div className="flex justify-center text-[#f6c85f]">
            <ArrowDown className="size-6 md:hidden" aria-hidden="true" />
            <ArrowRight className="hidden size-6 md:block" aria-hidden="true" />
          </div>

          <div className="rounded-xl border-2 border-[#f6c85f] bg-[#f7f8f4] p-4">
            <div className="flex items-center justify-between gap-3">
              <p className="m-0 font-mono text-[9px] font-black tracking-[0.15em] text-[#5e6d73]">
                FILE WRITE ORDER
              </p>
              <Gauge className="size-4 text-[#2f7d62]" aria-hidden="true" />
            </div>
            <div className="mt-4 grid grid-cols-5 gap-1.5">
              {["A", "B", "C", "D", "B"].map((chunk, index) => (
                <div
                  key={`${chunk}-${index}`}
                  className={cn(
                    "flex h-16 items-center justify-center rounded border border-[#45575e] font-mono text-lg font-black text-[#18252b]",
                    CHUNK_COLORS[chunk]
                  )}
                >
                  {chunk}
                </div>
              ))}
            </div>
          </div>
        </div>

        <p
          className="m-0 mt-6 border-t border-[#57707b] pt-4 text-sm leading-6 text-[#c1d2d8]"
          aria-live="polite"
        >
          {scenario.note}
        </p>
      </div>
    </figure>
  )
}

type GateScenario = {
  id: string
  label: string
  title: string
  checks: Array<{ label: string; state: "pass" | "fail" | "waiting" }>
  result: "published" | "blocked"
  temp: string
  reason: string
}

const GATE_SCENARIOS: GateScenario[] = [
  {
    id: "success",
    label: "Valid file",
    title: "Every proof agrees",
    checks: [
      { label: "5 / 5 recipe positions covered", state: "pass" },
      { label: "Every chunk matches its hash", state: "pass" },
      { label: "Size and full-file hash match", state: "pass" },
    ],
    result: "published",
    temp: "renamed over pointer",
    reason: "The sibling temporary file becomes the tracked file atomically.",
  },
  {
    id: "missing",
    label: "Missing term",
    title: "The plan cannot cover the file",
    checks: [
      { label: "4 / 5 recipe positions covered", state: "fail" },
      { label: "Chunk reads not started", state: "waiting" },
      { label: "Final verification not started", state: "waiting" },
    ],
    result: "blocked",
    temp: "not created",
    reason:
      "Coverage preflight stops before spending bandwidth on an incomplete plan.",
  },
  {
    id: "chunk",
    label: "Bad chunk",
    title: "One fetched range has the wrong bytes",
    checks: [
      { label: "5 / 5 recipe positions covered", state: "pass" },
      { label: "Chunk C hash mismatch", state: "fail" },
      { label: "Final verification not reached", state: "waiting" },
    ],
    result: "blocked",
    temp: "discarded",
    reason:
      "The bad range cannot enter the assembled file or local cache as valid content.",
  },
  {
    id: "final",
    label: "Wrong order",
    title: "Chunks are valid; the whole file is not",
    checks: [
      { label: "5 / 5 recipe positions covered", state: "pass" },
      { label: "Every chunk matches its hash", state: "pass" },
      { label: "Full-file BLAKE3 mismatch", state: "fail" },
    ],
    result: "blocked",
    temp: "discarded",
    reason:
      "Per-chunk checks cannot prove order. The final file hash closes that gap.",
  },
]

export function HydrationSafetyGate() {
  const [scenarioId, setScenarioId] = useState("success")
  const scenario =
    GATE_SCENARIOS.find((item) => item.id === scenarioId) ?? GATE_SCENARIOS[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(62rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden border-2 border-[#333b36] bg-[#f7f8f4] shadow-[8px_8px_0_#d5dbd5] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(62rem,calc(100vw-2rem))] lg:w-[min(62rem,calc(100vw-24.5rem))]">
      <header className="flex flex-wrap items-end justify-between gap-5 border-b-2 border-[#333b36] px-5 py-5 sm:px-7">
        <div>
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.19em] text-[#637067]">
            HYDRATION QUALITY GATE
          </p>
          <h3 className="m-0 mt-1 text-2xl font-black tracking-[-0.03em] text-[#202722]">
            The pointer moves only after the proof passes.
          </h3>
        </div>
        <div className="flex flex-wrap gap-1 border border-[#727d75] bg-white p-1">
          {GATE_SCENARIOS.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-pressed={scenario.id === item.id}
              onClick={() => setScenarioId(item.id)}
              className={cn(
                "px-3 py-1.5 font-mono text-[9px] font-black outline-none focus-visible:ring-2 focus-visible:ring-[#234e70]",
                scenario.id === item.id
                  ? "bg-[#333b36] text-white"
                  : "text-[#667069] hover:bg-[#edf0ec]"
              )}
            >
              {item.label}
            </button>
          ))}
        </div>
      </header>

      <div className="grid lg:grid-cols-[minmax(0,1fr)_18rem]">
        <div className="border-b-2 border-[#333b36] p-5 sm:p-7 lg:border-r-2 lg:border-b-0">
          <h4 className="m-0 text-xl font-black text-[#202722]">
            {scenario.title}
          </h4>
          <div className="mt-5 grid gap-2">
            {scenario.checks.map((check, index) => (
              <div
                key={check.label}
                className={cn(
                  "grid grid-cols-[2rem_minmax(0,1fr)_auto] items-center gap-3 border p-3",
                  check.state === "pass" && "border-[#83ab98] bg-[#e7f2eb]",
                  check.state === "fail" && "border-[#d39189] bg-[#f8e8e5]",
                  check.state === "waiting" &&
                    "border-[#c3c9c4] bg-[#eff1ee] text-[#7b837d]"
                )}
              >
                <span className="font-mono text-xs font-black">
                  {String(index + 1).padStart(2, "0")}
                </span>
                <span className="text-sm font-bold">{check.label}</span>
                {check.state === "pass" ? (
                  <Check
                    className="size-5 text-[#2f7d62]"
                    aria-label="Passed"
                  />
                ) : check.state === "fail" ? (
                  <CircleX
                    className="size-5 text-[#c84b45]"
                    aria-label="Failed"
                  />
                ) : (
                  <span className="font-mono text-[8px] font-black">WAIT</span>
                )}
              </div>
            ))}
          </div>

          <div className="mt-5 flex items-center gap-3 border-t border-dashed border-[#8a938c] pt-4">
            <PackageOpen className="size-5 text-[#5e6a62]" aria-hidden="true" />
            <p className="m-0 text-sm text-[#5e6a62]">
              Temporary output: <strong>{scenario.temp}</strong>
            </p>
          </div>
          <p className="m-0 mt-3 text-sm leading-6 text-[#5e6a62]">
            {scenario.reason}
          </p>
        </div>

        <aside
          className={cn(
            "flex flex-col justify-center p-6 text-center",
            scenario.result === "published"
              ? "bg-[#dff0e5] text-[#225e49]"
              : "bg-[#f7dfdb] text-[#973b32]"
          )}
          aria-live="polite"
        >
          <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em]">
            TRACKED PATH AFTER RUN
          </p>
          {scenario.result === "published" ? (
            <FileCheck2 className="mx-auto mt-5 size-14" aria-hidden="true" />
          ) : (
            <FileCode2 className="mx-auto mt-5 size-14" aria-hidden="true" />
          )}
          <p className="m-0 mt-4 text-3xl font-black tracking-[-0.05em]">
            {scenario.result === "published" ? "FULL FILE" : "POINTER"}
          </p>
          <div className="mt-5 border-t border-current/30 pt-4 font-mono text-[9px] font-black">
            {scenario.result === "published" ? (
              <span className="inline-flex items-center gap-2">
                <ShieldCheck className="size-4" aria-hidden="true" /> VERIFIED
              </span>
            ) : (
              <span className="inline-flex items-center gap-2">
                <RotateCcw className="size-4" aria-hidden="true" /> UNCHANGED
              </span>
            )}
          </div>
        </aside>
      </div>
    </figure>
  )
}
