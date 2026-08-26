"use client"

import {
  ArrowDown,
  Cloud,
  Coins,
  Database,
  Gauge,
  ReceiptText,
  RotateCcw,
  Wifi,
} from "lucide-react"
import { useState } from "react"

import { cn } from "@/lib/utils"

type CostScenario = {
  id: "standard" | "measured" | "cached" | "overarchive"
  label: string
  title: string
  availability: string
  note: string
  tiers: { label: string; gb: number; color: string }[]
  lines: { label: string; value: number; icon: typeof Cloud }[]
}

const COST_SCENARIOS: CostScenario[] = [
  {
    id: "standard",
    label: "All Standard",
    title: "Simple and immediately readable",
    availability: "DIRECT READ",
    note: "This is the baseline. Transfer, not storage, is already the largest line item.",
    tiers: [{ label: "Standard", gb: 10_000, color: "bg-[#4c3f91]" }],
    lines: [
      { label: "Storage", value: 230, icon: Database },
      { label: "Requests", value: 4, icon: Gauge },
      { label: "Retrieval", value: 0, icon: RotateCcw },
      { label: "Transfer", value: 180, icon: Wifi },
      { label: "Early deletion", value: 0, icon: Coins },
    ],
  },
  {
    id: "measured",
    label: "Measured tiers",
    title: "Keep the active 10% hot",
    availability: "MIXED LATENCY",
    note: "Tiering lowers storage, but recurring origin reads still dominate the bill.",
    tiers: [
      { label: "Hot", gb: 1_000, color: "bg-[#4c3f91]" },
      { label: "IA", gb: 3_000, color: "bg-[#43b3ae]" },
      { label: "Glacier IR", gb: 6_000, color: "bg-[#c7d66d]" },
    ],
    lines: [
      { label: "Storage", value: 84.5, icon: Database },
      { label: "Requests", value: 7, icon: Gauge },
      { label: "Retrieval", value: 12, icon: RotateCcw },
      { label: "Transfer", value: 180, icon: Wifi },
      { label: "Early deletion", value: 0, icon: Coins },
    ],
  },
  {
    id: "cached",
    label: "Tiers + cache",
    title: "Serve repeat reads before origin",
    availability: "FAST WHEN WARM",
    note: "The cache does not reduce origin storage. It cuts repeated retrieval and transfer.",
    tiers: [
      { label: "Hot", gb: 1_000, color: "bg-[#4c3f91]" },
      { label: "IA", gb: 3_000, color: "bg-[#43b3ae]" },
      { label: "Glacier IR", gb: 6_000, color: "bg-[#c7d66d]" },
    ],
    lines: [
      { label: "Storage", value: 84.5, icon: Database },
      { label: "Requests", value: 5, icon: Gauge },
      { label: "Retrieval", value: 4, icon: RotateCcw },
      { label: "Transfer", value: 54, icon: Wifi },
      { label: "Early deletion", value: 0, icon: Coins },
    ],
  },
  {
    id: "overarchive",
    label: "Archive too early",
    title: "The lowest storage rate loses",
    availability: "RESTORE REQUIRED",
    note: "Retrieval, delay, and one early-deletion event erase most of the storage win.",
    tiers: [
      { label: "Hot", gb: 1_000, color: "bg-[#4c3f91]" },
      { label: "IA", gb: 1_000, color: "bg-[#43b3ae]" },
      { label: "Deep Archive", gb: 8_000, color: "bg-[#e56b6f]" },
    ],
    lines: [
      { label: "Storage", value: 43.42, icon: Database },
      { label: "Requests", value: 12, icon: Gauge },
      { label: "Retrieval", value: 40, icon: RotateCcw },
      { label: "Transfer", value: 180, icon: Wifi },
      { label: "Early deletion", value: 60, icon: Coins },
    ],
  },
]

export function CostReceiptMixer() {
  const [scenarioId, setScenarioId] = useState<CostScenario["id"]>("standard")
  const selected =
    COST_SCENARIOS.find((item) => item.id === scenarioId) ?? COST_SCENARIOS[0]
  const total = selected.lines.reduce((sum, line) => sum + line.value, 0)
  const baseline = COST_SCENARIOS[0].lines.reduce(
    (sum, line) => sum + line.value,
    0
  )
  const measured = COST_SCENARIOS[1].lines.reduce(
    (sum, line) => sum + line.value,
    0
  )
  const difference = baseline - total
  const archiveOverage = total - measured

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.75rem] border border-[#a9b3ad] bg-[#dce7e4] text-[#18212a] shadow-[0_20px_60px_rgba(24,33,42,0.16)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="grid gap-5 border-b border-[#a9b3ad] px-5 py-5 sm:px-7 lg:grid-cols-[1fr_auto] lg:items-end">
        <div>
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-[#4c3f91]">
            MONTHLY COST RECEIPT / 10,000 GB / 2,000 GB READ
          </p>
          <h3 className="m-0 mt-2 text-2xl font-black tracking-[-0.04em] sm:text-3xl">
            Which line item did the policy move?
          </h3>
        </div>
        <div className="flex flex-wrap gap-2" aria-label="Cost scenario">
          {COST_SCENARIOS.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-pressed={selected.id === item.id}
              onClick={() => setScenarioId(item.id)}
              className={cn(
                "min-h-11 rounded-full border px-3 py-2 font-mono text-[10px] font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#4c3f91] focus-visible:ring-offset-2 focus-visible:ring-offset-[#dce7e4]",
                selected.id === item.id
                  ? "border-[#18212a] bg-[#18212a] text-white"
                  : "border-[#87958e] bg-[#f7f8f0] text-[#52615b] hover:border-[#18212a] hover:text-[#18212a]"
              )}
            >
              {item.label}
            </button>
          ))}
        </div>
      </header>

      <div className="grid lg:grid-cols-[1fr_22rem]" aria-live="polite">
        <section className="border-b border-[#a9b3ad] p-5 sm:p-7 lg:border-r lg:border-b-0">
          <div className="flex flex-wrap items-start justify-between gap-4">
            <div>
              <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#60716a]">
                REPOSITORY PLACEMENT
              </p>
              <h4 className="m-0 mt-1 text-xl font-black sm:text-2xl">
                {selected.title}
              </h4>
            </div>
            <span className="rounded-full bg-[#f7f8f0] px-3 py-2 font-mono text-[9px] font-black text-[#4c3f91]">
              {selected.availability}
            </span>
          </div>

          <div className="mt-6 overflow-hidden rounded-2xl border-2 border-[#18212a] bg-[#f7f8f0] p-1">
            <div className="flex h-24 gap-1">
              {selected.tiers.map((tier) => (
                <div
                  key={tier.label}
                  className={cn(
                    "flex min-w-16 flex-col justify-end rounded-xl p-3 text-white",
                    tier.color
                  )}
                  style={{ width: `${(tier.gb / 10_000) * 100}%` }}
                >
                  <p className="m-0 text-xs font-black">{tier.label}</p>
                  <p className="m-0 mt-1 font-mono text-[9px] text-white/80">
                    {tier.gb.toLocaleString()} GB
                  </p>
                </div>
              ))}
            </div>
          </div>

          <div className="mt-6 grid gap-3 sm:grid-cols-2 xl:grid-cols-5">
            {selected.lines.map((line) => {
              const Icon = line.icon
              return (
                <div
                  key={line.label}
                  className="rounded-xl border border-[#b5bfba] bg-[#f7f8f0] p-3"
                >
                  <Icon className="size-4 text-[#4c3f91]" aria-hidden="true" />
                  <p className="m-0 mt-4 text-xs font-black">{line.label}</p>
                  <p className="m-0 mt-1 font-mono text-[11px] text-[#60716a]">
                    ${line.value.toFixed(2)}
                  </p>
                </div>
              )
            })}
          </div>

          <div className="mt-6 flex gap-3 rounded-xl border border-dashed border-[#87958e] bg-[#edf2ee] p-4">
            <ArrowDown
              className="mt-0.5 size-5 shrink-0 text-[#e56b6f]"
              aria-hidden="true"
            />
            <p className="m-0 text-sm leading-6 text-[#52615b]">
              {selected.note}
            </p>
          </div>
        </section>

        <aside className="relative bg-[#f7f8f0] p-6 font-mono sm:p-7">
          <div className="absolute top-0 left-0 hidden h-full w-3 -translate-x-1/2 bg-[radial-gradient(circle,#18212a_4px,transparent_5px)] bg-[length:12px_24px] lg:block" />
          <div className="flex items-center justify-between gap-3 border-b border-dashed border-[#87958e] pb-4">
            <div>
              <p className="m-0 text-[9px] font-black tracking-[0.16em] text-[#60716a]">
                ESTIMATE
              </p>
              <p className="m-0 mt-1 text-sm font-black">crab / monthly</p>
            </div>
            <ReceiptText className="size-6 text-[#4c3f91]" aria-hidden="true" />
          </div>

          <div className="mt-5 space-y-3 text-[10px]">
            {selected.lines.map((line) => (
              <div
                key={line.label}
                className="flex items-center justify-between gap-4"
              >
                <span className="text-[#52615b]">{line.label}</span>
                <span>${line.value.toFixed(2)}</span>
              </div>
            ))}
          </div>

          <div className="mt-5 border-y-2 border-[#18212a] py-4">
            <div className="flex items-end justify-between gap-4">
              <span className="text-xs font-black">TOTAL</span>
              <span className="text-4xl font-black tracking-[-0.08em]">
                ${total.toFixed(2)}
              </span>
            </div>
          </div>

          <div
            className={cn(
              "mt-5 rounded-xl p-4 text-xs font-black",
              selected.id === "overarchive"
                ? "bg-[#f6dddd] text-[#7b3038]"
                : difference > 0
                  ? "bg-[#e6edc5] text-[#435116]"
                  : "bg-[#ece9f7] text-[#4c3f91]"
            )}
          >
            {selected.id === "overarchive"
              ? `$${archiveOverage.toFixed(2)} above measured tiers`
              : difference > 0
                ? `$${difference.toFixed(2)} below baseline`
                : "baseline"}
          </div>

          <p className="m-0 mt-6 text-[9px] leading-4 text-[#60716a]">
            Illustration uses Crab’s embedded S3 us-east-1 rate table. Replace
            with your region or contract rates.
          </p>
        </aside>
      </div>
    </figure>
  )
}
