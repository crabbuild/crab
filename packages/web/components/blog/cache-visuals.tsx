"use client"

import {
  ArrowDown,
  ArrowRight,
  Check,
  Cloud,
  Database,
  HardDrive,
  Server,
  ShieldCheck,
} from "lucide-react"
import { useState } from "react"

import { cn } from "@/lib/utils"

type RouteStep = {
  id: "local" | "shared" | "origin"
  label: string
  note: string
  outcome: "hit" | "miss" | "skip"
}

type RouteScenario = {
  id: string
  label: string
  title: string
  result: string
  explanation: string
  steps: RouteStep[]
}

const ROUTE_SCENARIOS: RouteScenario[] = [
  {
    id: "cold",
    label: "First read",
    title: "Nothing nearby has the bytes yet",
    result: "ORIGIN",
    explanation:
      "The verified origin response fills the local cache for the next read.",
    steps: [
      { id: "local", label: "Local disk", note: "empty", outcome: "miss" },
      {
        id: "shared",
        label: "Team cache",
        note: "not configured or empty",
        outcome: "miss",
      },
      {
        id: "origin",
        label: "Object storage",
        note: "canonical bytes",
        outcome: "hit",
      },
    ],
  },
  {
    id: "warm",
    label: "Repeat read",
    title: "The same machine already has verified bytes",
    result: "LOCAL",
    explanation: "The request stops at local disk. No network read is needed.",
    steps: [
      {
        id: "local",
        label: "Local disk",
        note: "verified entry",
        outcome: "hit",
      },
      {
        id: "shared",
        label: "Team cache",
        note: "not contacted",
        outcome: "skip",
      },
      {
        id: "origin",
        label: "Object storage",
        note: "not contacted",
        outcome: "skip",
      },
    ],
  },
  {
    id: "teammate",
    label: "Teammate",
    title: "This machine is cold; the team cache is warm",
    result: "TEAM",
    explanation:
      "The shared response is verified, then written to this machine's local cache.",
    steps: [
      { id: "local", label: "Local disk", note: "empty", outcome: "miss" },
      {
        id: "shared",
        label: "Team cache",
        note: "verified response",
        outcome: "hit",
      },
      {
        id: "origin",
        label: "Object storage",
        note: "not contacted",
        outcome: "skip",
      },
    ],
  },
  {
    id: "outage",
    label: "Cache offline",
    title: "Acceleration failed; correctness did not",
    result: "ORIGIN",
    explanation:
      "An unavailable team cache is bypassed. Canonical object storage still serves the read.",
    steps: [
      { id: "local", label: "Local disk", note: "empty", outcome: "miss" },
      {
        id: "shared",
        label: "Team cache",
        note: "unavailable",
        outcome: "miss",
      },
      {
        id: "origin",
        label: "Object storage",
        note: "canonical bytes",
        outcome: "hit",
      },
    ],
  },
]

const ROUTE_ICONS = {
  local: HardDrive,
  shared: Server,
  origin: Cloud,
}

export function CacheRouteTicket() {
  const [scenarioId, setScenarioId] = useState("cold")
  const scenario =
    ROUTE_SCENARIOS.find((item) => item.id === scenarioId) ?? ROUTE_SCENARIOS[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.75rem] bg-[#071d2b] text-[#eaf9ff] shadow-[0_18px_60px_rgba(7,29,43,0.2)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="grid gap-5 border-b border-[#34505f] px-5 py-5 sm:px-7 lg:grid-cols-[1fr_auto] lg:items-end">
        <div>
          <p className="m-0 font-mono text-[10px] font-bold tracking-[0.2em] text-[#7edcf2]">
            READ DISPATCH / ONE IMMUTABLE OBJECT
          </p>
          <h3 className="m-0 mt-2 text-2xl font-black tracking-[-0.03em] text-white sm:text-3xl">
            How far does this read travel?
          </h3>
        </div>
        <div className="flex flex-wrap gap-2" aria-label="Read scenario">
          {ROUTE_SCENARIOS.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-pressed={scenario.id === item.id}
              onClick={() => setScenarioId(item.id)}
              className={cn(
                "rounded-full border px-3 py-1.5 font-mono text-[10px] font-bold transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#7edcf2] focus-visible:ring-offset-2 focus-visible:ring-offset-[#071d2b]",
                scenario.id === item.id
                  ? "border-[#7edcf2] bg-[#7edcf2] text-[#071d2b]"
                  : "border-[#496572] text-[#b8ced7] hover:border-[#7edcf2] hover:text-white"
              )}
            >
              {item.label}
            </button>
          ))}
        </div>
      </header>

      <div className="grid lg:grid-cols-[minmax(0,1fr)_17rem]">
        <div className="p-5 sm:p-7">
          <div className="grid gap-3 sm:grid-cols-[1fr_auto_1fr_auto_1fr] sm:items-center">
            {scenario.steps.map((step, index) => {
              const Icon = ROUTE_ICONS[step.id]
              return (
                <div key={step.id} className="contents">
                  <div
                    className={cn(
                      "relative min-h-36 rounded-2xl border p-4 transition-colors duration-300 motion-reduce:transition-none",
                      step.outcome === "hit" &&
                        "border-[#7edcf2] bg-[#103b49] shadow-[inset_0_0_0_1px_#7edcf2]",
                      step.outcome === "miss" &&
                        "border-[#d8a449] bg-[#282b2b]",
                      step.outcome === "skip" &&
                        "border-[#34505f] bg-[#0b2533] opacity-55"
                    )}
                  >
                    <div className="flex items-start justify-between gap-3">
                      <Icon className="size-5" aria-hidden="true" />
                      <span
                        className={cn(
                          "rounded-full px-2 py-1 font-mono text-[9px] font-black",
                          step.outcome === "hit" &&
                            "bg-[#7edcf2] text-[#071d2b]",
                          step.outcome === "miss" &&
                            "bg-[#d8a449] text-[#241b0b]",
                          step.outcome === "skip" &&
                            "bg-[#29424e] text-[#91aab5]"
                        )}
                      >
                        {step.outcome.toUpperCase()}
                      </span>
                    </div>
                    <p className="m-0 mt-7 text-base font-black text-white">
                      {step.label}
                    </p>
                    <p className="m-0 mt-1 text-xs leading-5 text-[#a9c1cb]">
                      {step.note}
                    </p>
                  </div>
                  {index < scenario.steps.length - 1 ? (
                    <div className="flex justify-center text-[#68828e] sm:block">
                      <ArrowDown
                        className="size-5 sm:hidden"
                        aria-hidden="true"
                      />
                      <ArrowRight
                        className="hidden size-5 sm:block"
                        aria-hidden="true"
                      />
                    </div>
                  ) : null}
                </div>
              )
            })}
          </div>
        </div>

        <aside
          className="relative border-t border-dashed border-[#5c7681] bg-[#e9f8fc] p-6 text-[#071d2b] lg:border-t-0 lg:border-l"
          aria-live="polite"
        >
          <div className="absolute top-0 left-0 hidden h-full w-3 -translate-x-1/2 bg-[radial-gradient(circle,#071d2b_4px,transparent_5px)] bg-[length:12px_24px] lg:block" />
          <p className="m-0 font-mono text-[9px] font-black tracking-[0.18em] text-[#4d6671]">
            SERVED BY
          </p>
          <p className="m-0 mt-2 font-mono text-5xl leading-none font-black tracking-[-0.07em] text-[#0b7189]">
            {scenario.result}
          </p>
          <h4 className="m-0 mt-7 text-lg leading-6 font-black">
            {scenario.title}
          </h4>
          <p className="m-0 mt-2 text-sm leading-6 text-[#4d6671]">
            {scenario.explanation}
          </p>
          <div className="mt-7 border-t border-[#aac7d0] pt-4 font-mono text-[9px] font-bold text-[#4d6671]">
            CONTENT ID CHECKED
            <Check
              className="ml-2 inline size-4 text-[#0b8c6e]"
              aria-hidden="true"
            />
          </div>
        </aside>
      </div>
    </figure>
  )
}

type RangePattern = {
  id: string
  label: string
  title: string
  required: number[]
  groups: number[][]
  exactBytes: number
  plannedBytes: number
  note: string
}

const RANGE_PATTERNS: RangePattern[] = [
  {
    id: "adjacent",
    label: "Adjacent",
    title: "Three neighboring chunks",
    required: [1, 2, 3],
    groups: [[1, 2, 3]],
    exactBytes: 24,
    plannedBytes: 24,
    note: "One contiguous range replaces three small requests with no extra bytes.",
  },
  {
    id: "small-gap",
    label: "Small gap",
    title: "Useful chunks around one gap",
    required: [1, 3, 4],
    groups: [[1, 2, 3, 4]],
    exactBytes: 24,
    plannedBytes: 32,
    note: "One wider read spends 8 MiB to remove two request round trips.",
  },
  {
    id: "sparse",
    label: "Sparse",
    title: "Two distant chunks",
    required: [0, 6],
    groups: [[0], [6]],
    exactBytes: 16,
    plannedBytes: 16,
    note: "The gap is too wide. Two narrow ranges avoid downloading unused bytes.",
  },
]

export function RangePlanningTape() {
  const [patternId, setPatternId] = useState("adjacent")
  const pattern =
    RANGE_PATTERNS.find((item) => item.id === patternId) ?? RANGE_PATTERNS[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden border border-[#263a4a] bg-[#f4f7f8] shadow-[0_12px_40px_rgba(38,58,74,0.14)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="flex flex-wrap items-end justify-between gap-5 border-b border-[#aab9c0] bg-white px-5 py-5 sm:px-7">
        <div>
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.18em] text-[#6b7e87]">
            RANGE PLANNING TAPE / ILLUSTRATIVE SCALE
          </p>
          <h3 className="m-0 mt-1 text-2xl font-black tracking-[-0.03em] text-[#172934]">
            Fewer requests can mean more bytes.
          </h3>
        </div>
        <div className="flex border border-[#263a4a] bg-[#edf2f4] p-1">
          {RANGE_PATTERNS.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-pressed={pattern.id === item.id}
              onClick={() => setPatternId(item.id)}
              className={cn(
                "px-3 py-1.5 font-mono text-[10px] font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#137d89]",
                pattern.id === item.id
                  ? "bg-[#172934] text-white"
                  : "text-[#5f727b] hover:bg-white"
              )}
            >
              {item.label}
            </button>
          ))}
        </div>
      </header>

      <div className="p-5 sm:p-7">
        <div className="flex items-center justify-between gap-4">
          <h4 className="m-0 text-lg font-black text-[#172934]">
            {pattern.title}
          </h4>
          <span className="font-mono text-[10px] font-bold text-[#6b7e87]">
            ONE XORB →
          </span>
        </div>

        <div
          className="mt-5 grid grid-cols-8 gap-1"
          aria-label="Toy xorb ranges"
        >
          {Array.from({ length: 8 }, (_, index) => {
            const required = pattern.required.includes(index)
            const fetched = pattern.groups.some((group) =>
              group.includes(index)
            )
            return (
              <div
                key={index}
                className={cn(
                  "relative flex h-24 items-end justify-center border pb-3 font-mono text-[10px] font-black transition-colors duration-300 motion-reduce:transition-none",
                  required && "border-[#087f76] bg-[#65d6c6] text-[#073d3a]",
                  !required &&
                    fetched &&
                    "border-[#d6962d] bg-[repeating-linear-gradient(135deg,#f7c96e_0,#f7c96e_6px,#ffe2a6_6px,#ffe2a6_12px)] text-[#6b4300]",
                  !fetched && "border-[#bac6cb] bg-white text-[#8b9ba2]"
                )}
              >
                <span>{index}</span>
                {required ? (
                  <span className="absolute top-2 left-1/2 -translate-x-1/2 text-[8px]">
                    NEED
                  </span>
                ) : null}
                {!required && fetched ? (
                  <span className="absolute top-2 left-1/2 -translate-x-1/2 text-[8px]">
                    GAP
                  </span>
                ) : null}
              </div>
            )
          })}
        </div>

        <div className="mt-6 grid gap-px overflow-hidden border border-[#263a4a] bg-[#263a4a] sm:grid-cols-2">
          <div className="bg-white p-5">
            <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#6b7e87]">
              ONE REQUEST PER NEEDED CHUNK
            </p>
            <div className="mt-4 flex items-end justify-between gap-4">
              <div>
                <p className="m-0 text-4xl font-black text-[#172934]">
                  {pattern.required.length}
                </p>
                <p className="m-0 text-xs text-[#6b7e87]">requests</p>
              </div>
              <div className="text-right">
                <p className="m-0 text-2xl font-black text-[#172934]">
                  {pattern.exactBytes} MiB
                </p>
                <p className="m-0 text-xs text-[#6b7e87]">transferred</p>
              </div>
            </div>
          </div>
          <div className="bg-[#e8f7f4] p-5">
            <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#47726d]">
              PLANNED CONTIGUOUS RANGES
            </p>
            <div className="mt-4 flex items-end justify-between gap-4">
              <div>
                <p className="m-0 text-4xl font-black text-[#087f76]">
                  {pattern.groups.length}
                </p>
                <p className="m-0 text-xs text-[#47726d]">requests</p>
              </div>
              <div className="text-right">
                <p className="m-0 text-2xl font-black text-[#087f76]">
                  {pattern.plannedBytes} MiB
                </p>
                <p className="m-0 text-xs text-[#47726d]">transferred</p>
              </div>
            </div>
          </div>
        </div>

        <p
          className="m-0 mt-5 border-l-4 border-[#d6962d] pl-4 text-sm leading-6 text-[#52666f]"
          aria-live="polite"
        >
          {pattern.note}
        </p>
      </div>
    </figure>
  )
}

type ObjectPassport = {
  id: string
  label: string
  object: string
  identity: string
  changes: string
  route: string
  stamp: string
  cacheable: boolean
  note: string
}

const OBJECT_PASSPORTS: ObjectPassport[] = [
  {
    id: "xorb",
    label: "Xorb",
    object: "Packed chunk bytes",
    identity: "Content hash",
    changes: "Never under the same identity",
    route: "Local → team → origin",
    stamp: "CACHEABLE",
    cacheable: true,
    note: "Wrong bytes fail verification and the client repairs from origin.",
  },
  {
    id: "shard",
    label: "Shard",
    object: "Chunk placement metadata",
    identity: "Content-addressed path",
    changes: "New content gets a new object",
    route: "Local → team → origin",
    stamp: "CACHEABLE",
    cacheable: true,
    note: "Cached metadata can locate immutable bytes without changing repository state.",
  },
  {
    id: "ref",
    label: "Ref",
    object: "Visible branch position",
    identity: "Mutable repository name",
    changes: "Moves when a push publishes",
    route: "Origin directly",
    stamp: "BYPASS",
    cacheable: false,
    note: "A cached ref could hide a newer commit, so authority stays at origin.",
  },
]

export function CacheObjectPassport() {
  const [passportId, setPassportId] = useState("xorb")
  const passport =
    OBJECT_PASSPORTS.find((item) => item.id === passportId) ??
    OBJECT_PASSPORTS[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(58rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-3xl border border-[#d1c9bb] bg-[#f2eee5] p-3 shadow-[0_15px_45px_rgba(55,43,28,0.13)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(58rem,calc(100vw-2rem))] lg:w-[min(58rem,calc(100vw-24.5rem))]">
      <div className="rounded-[1.1rem] border border-[#9e9587] bg-[#fffdf8] p-5 sm:p-7">
        <header className="flex flex-wrap items-start justify-between gap-5 border-b border-dashed border-[#aaa091] pb-5">
          <div>
            <p className="m-0 font-mono text-[10px] font-black tracking-[0.18em] text-[#786e61]">
              CACHE ADMISSION PASSPORT
            </p>
            <h3 className="m-0 mt-1 text-2xl font-black tracking-[-0.03em] text-[#2d2923]">
              Immutable bytes travel. Mutable truth does not.
            </h3>
          </div>
          <div className="flex gap-1 rounded-full border border-[#9e9587] p-1">
            {OBJECT_PASSPORTS.map((item) => (
              <button
                key={item.id}
                type="button"
                aria-pressed={passport.id === item.id}
                onClick={() => setPassportId(item.id)}
                className={cn(
                  "rounded-full px-3 py-1.5 font-mono text-[10px] font-black outline-none focus-visible:ring-2 focus-visible:ring-[#176c86]",
                  passport.id === item.id
                    ? "bg-[#2d2923] text-white"
                    : "text-[#786e61] hover:bg-[#eee8dd]"
                )}
              >
                {item.label}
              </button>
            ))}
          </div>
        </header>

        <div className="grid gap-7 pt-6 sm:grid-cols-[minmax(0,1fr)_13rem] sm:items-stretch">
          <dl className="m-0 grid gap-px overflow-hidden border border-[#c7bdad] bg-[#c7bdad] sm:grid-cols-2">
            {[
              ["OBJECT", passport.object],
              ["IDENTITY", passport.identity],
              ["CAN IT CHANGE?", passport.changes],
              ["READ ROUTE", passport.route],
            ].map(([label, value]) => (
              <div key={label} className="bg-[#fffdf8] p-4">
                <dt className="font-mono text-[9px] font-black tracking-[0.15em] text-[#84796b]">
                  {label}
                </dt>
                <dd className="m-0 mt-2 text-sm leading-5 font-bold text-[#2d2923]">
                  {value}
                </dd>
              </div>
            ))}
          </dl>

          <div
            className={cn(
              "flex min-h-44 rotate-[-2deg] flex-col items-center justify-center border-4 p-4 text-center",
              passport.cacheable
                ? "border-[#16785f] bg-[#e6f4e9] text-[#16785f]"
                : "border-[#b44d3a] bg-[#fae8e2] text-[#b44d3a]"
            )}
          >
            {passport.cacheable ? (
              <ShieldCheck className="size-9" aria-hidden="true" />
            ) : (
              <Database className="size-9" aria-hidden="true" />
            )}
            <p className="m-0 mt-3 font-mono text-2xl font-black tracking-[-0.05em]">
              {passport.stamp}
            </p>
            <p className="m-0 mt-1 font-mono text-[9px] font-black tracking-[0.12em]">
              {passport.cacheable ? "VERIFY ON READ" : "ASK THE AUTHORITY"}
            </p>
          </div>
        </div>

        <p
          className="m-0 mt-6 border-t border-dashed border-[#aaa091] pt-4 text-sm leading-6 text-[#655c50]"
          aria-live="polite"
        >
          {passport.note}
        </p>
      </div>
    </figure>
  )
}
