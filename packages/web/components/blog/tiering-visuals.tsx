"use client"

import {
  ArchiveRestore,
  ArrowRight,
  Check,
  Clock3,
  FileArchive,
  FileCog,
  Flame,
  GitCommitHorizontal,
  PackageOpen,
  Snowflake,
  X,
} from "lucide-react"
import { useState } from "react"

import { cn } from "@/lib/utils"

type TierObject = {
  id: "new-xorb" | "warm-xorb" | "cold-xorb" | "shard" | "ref" | "pack"
  label: string
  path: string
  age: string
  eligible: boolean
  destination: string
  reason: string
  icon: typeof FileArchive
  structures: string[]
}

const TIER_OBJECTS: TierObject[] = [
  {
    id: "new-xorb",
    label: "10-day xorb",
    path: ".crab/xorbs/a1/a1f4…",
    age: "10 days",
    eligible: true,
    destination: "STANDARD",
    reason:
      "The object matches the xorb prefix but has not reached the 30-day transition.",
    icon: Flame,
    structures: ["object prefix", "last modified", "30-day rule"],
  },
  {
    id: "warm-xorb",
    label: "75-day xorb",
    path: ".crab/xorbs/b7/b72c…",
    age: "75 days",
    eligible: true,
    destination: "INFREQUENT ACCESS",
    reason:
      "It passed the 30-day warm transition but not the 180-day cold transition.",
    icon: PackageOpen,
    structures: ["xorb prefix", "transition rule", "storage class"],
  },
  {
    id: "cold-xorb",
    label: "240-day xorb",
    path: ".crab/xorbs/c9/c90d…",
    age: "240 days",
    eligible: true,
    destination: "DEEP COLD",
    reason:
      "It passed both default age thresholds. Access evidence must still justify the delay.",
    icon: Snowflake,
    structures: ["xorb prefix", "180-day rule", "restore policy"],
  },
  {
    id: "shard",
    label: "240-day shard",
    path: ".crab/shards/3f/3f18…",
    age: "240 days",
    eligible: false,
    destination: "KEEP READABLE",
    reason:
      "Shards map file and chunk identities to xorbs. Hydration planning needs them immediately.",
    icon: FileCog,
    structures: ["shard metadata", "file recipe", "range planning"],
  },
  {
    id: "ref",
    label: "Repository ref",
    path: "repo/vision/refs/main",
    age: "current",
    eligible: false,
    destination: "KEEP READABLE",
    reason:
      "Refs are mutable repository control data, not tier-eligible content objects.",
    icon: GitCommitHorizontal,
    structures: [
      "ref transaction",
      "current generation",
      "repository discovery",
    ],
  },
  {
    id: "pack",
    label: "Git pack",
    path: "repo/vision/packs/pack-18.pack",
    age: "210 days",
    eligible: false,
    destination: "KEEP READABLE",
    reason: "Git packs remain outside Crab’s xorb-only lifecycle rule.",
    icon: FileArchive,
    structures: ["Git pack", "fetch path", "lifecycle prefix"],
  },
]

export function TierEligibilitySorter() {
  const [objectId, setObjectId] = useState<TierObject["id"]>("new-xorb")
  const selected =
    TIER_OBJECTS.find((item) => item.id === objectId) ?? TIER_OBJECTS[0]
  const Icon = selected.icon

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.75rem] bg-[#18212a] text-white shadow-[0_20px_60px_rgba(24,33,42,0.2)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="border-b border-[#56626d] px-5 py-5 sm:px-7">
        <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-[#c7d66d]">
          LIFECYCLE SORTER / SELECT AN OBJECT
        </p>
        <div className="mt-3 flex flex-wrap gap-2" aria-label="Storage object">
          {TIER_OBJECTS.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-pressed={selected.id === item.id}
              onClick={() => setObjectId(item.id)}
              className={cn(
                "min-h-11 rounded-lg border px-3 py-2 font-mono text-[10px] font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#c7d66d] focus-visible:ring-offset-2 focus-visible:ring-offset-[#18212a]",
                selected.id === item.id
                  ? "border-[#c7d66d] bg-[#c7d66d] text-[#18212a]"
                  : "border-[#56626d] text-[#c5ced2] hover:border-[#c7d66d] hover:text-white"
              )}
            >
              {item.label}
            </button>
          ))}
        </div>
      </header>

      <div className="p-5 sm:p-7" aria-live="polite">
        <div className="grid gap-3 lg:grid-cols-[1fr_auto_1fr_auto_1fr] lg:items-stretch">
          <div className="rounded-2xl border border-[#56626d] bg-[#24313b] p-5">
            <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#81919a]">
              INVENTORY OBJECT
            </p>
            <Icon className="mt-6 size-7 text-[#43b3ae]" aria-hidden="true" />
            <p className="m-0 mt-5 font-mono text-sm font-black break-all">
              {selected.path}
            </p>
            <p className="m-0 mt-2 font-mono text-[10px] text-[#aab8be]">
              age: {selected.age}
            </p>
          </div>

          <ArrowRight
            className="mx-auto size-5 rotate-90 self-center text-[#81919a] lg:rotate-0"
            aria-hidden="true"
          />

          <div
            className={cn(
              "rounded-2xl border-2 p-5",
              selected.eligible
                ? "border-[#43b3ae] bg-[#20403f]"
                : "border-[#e56b6f] bg-[#442d35]"
            )}
          >
            <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#aab8be]">
              PREFIX GATE
            </p>
            <div className="mt-7 flex items-center gap-3">
              {selected.eligible ? (
                <Check className="size-7 text-[#43b3ae]" aria-hidden="true" />
              ) : (
                <X className="size-7 text-[#e56b6f]" aria-hidden="true" />
              )}
              <p className="m-0 text-xl font-black">
                {selected.eligible ? "XORB ELIGIBLE" : "METADATA EXCLUDED"}
              </p>
            </div>
            <p className="m-0 mt-3 font-mono text-[10px] text-[#c5ced2]">
              rule: .crab/xorbs/ only
            </p>
          </div>

          <ArrowRight
            className="mx-auto size-5 rotate-90 self-center text-[#81919a] lg:rotate-0"
            aria-hidden="true"
          />

          <div className="rounded-2xl border border-[#56626d] bg-[#f7f8f0] p-5 text-[#18212a]">
            <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#60716a]">
              DESTINATION
            </p>
            <p className="m-0 mt-7 font-mono text-2xl font-black tracking-[-0.05em] text-[#4c3f91]">
              {selected.destination}
            </p>
            <p className="m-0 mt-3 text-sm leading-6 text-[#52615b]">
              {selected.reason}
            </p>
          </div>
        </div>

        <div className="mt-6 flex flex-wrap items-center gap-2 border-t border-dashed border-[#56626d] pt-5">
          <span className="mr-2 font-mono text-[9px] font-black tracking-[0.16em] text-[#81919a]">
            KEY EVIDENCE
          </span>
          {selected.structures.map((structure) => (
            <span
              key={structure}
              className="rounded-full border border-[#56626d] bg-[#24313b] px-3 py-1.5 font-mono text-[10px] text-[#c5ced2]"
            >
              {structure}
            </span>
          ))}
        </div>
      </div>
    </figure>
  )
}

type RestoreCase = {
  id: "warm" | "ci" | "nightly" | "incident"
  label: string
  title: string
  storageClass: string
  policy: string
  command: string
  result: string
  timing: string
  note: string
  tone: "direct" | "blocked" | "wait" | "urgent"
}

const RESTORE_CASES: RestoreCase[] = [
  {
    id: "warm",
    label: "Warm model",
    title: "Read immediately",
    storageClass: "Standard-IA",
    policy: "direct read",
    command: "crab hydrate 'models/current/**'",
    result: "BYTES READABLE",
    timing: "no restore step",
    note: "Retrieval fees may apply, but the object does not need provider restore staging.",
    tone: "direct",
  },
  {
    id: "ci",
    label: "CI fail-fast",
    title: "Do not turn a test into a restore job",
    storageClass: "Deep Archive",
    policy: "restore disabled",
    command: "crab hydrate --all --no-restore",
    result: "FAIL WITH DIRECTION",
    timing: "immediate decision",
    note: "Use this when the job must fail rather than wait or create restore charges.",
    tone: "blocked",
  },
  {
    id: "nightly",
    label: "Nightly batch",
    title: "Trade time for a cheaper restore",
    storageClass: "Glacier Flexible",
    policy: "bulk restore",
    command:
      "crab hydrate --all --restore-tier=bulk --restore-duration-days=14",
    result: "RESTORE + HYDRATE",
    timing: "provider-dependent wait",
    note: "The restored copy stays readable for 14 days, useful for a scheduled batch window.",
    tone: "wait",
  },
  {
    id: "incident",
    label: "Incident recovery",
    title: "Pay for the fastest supported path",
    storageClass: "Glacier Flexible",
    policy: "expedited restore",
    command: "crab hydrate --all --restore-tier=expedited",
    result: "URGENT RESTORE",
    timing: "S3 example: 1–5 min",
    note: "Run recovery drills before relying on this objective; provider capacity and class rules still apply.",
    tone: "urgent",
  },
]

const RESTORE_TONE = {
  direct: "border-[#43b3ae] bg-[#d9efea] text-[#205a55]",
  blocked: "border-[#e56b6f] bg-[#f6dddd] text-[#7b3038]",
  wait: "border-[#4c3f91] bg-[#e8e4f5] text-[#4c3f91]",
  urgent: "border-[#e8a933] bg-[#fff0c9] text-[#70500f]",
}

export function RestoreDispatchBoard() {
  const [caseId, setCaseId] = useState<RestoreCase["id"]>("warm")
  const selected =
    RESTORE_CASES.find((item) => item.id === caseId) ?? RESTORE_CASES[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.75rem] border border-[#a9b3ad] bg-[#f7f8f0] text-[#18212a] shadow-[0_20px_60px_rgba(24,33,42,0.14)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="grid gap-5 border-b border-[#a9b3ad] px-5 py-5 sm:px-7 lg:grid-cols-[1fr_auto] lg:items-end">
        <div>
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-[#4c3f91]">
            RESTORE DISPATCH / SELECT A WORKLOAD
          </p>
          <h3 className="m-0 mt-2 text-2xl font-black tracking-[-0.04em] sm:text-3xl">
            What should happen when cold data is requested?
          </h3>
        </div>
        <div className="flex flex-wrap gap-2" aria-label="Restore workload">
          {RESTORE_CASES.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-pressed={selected.id === item.id}
              onClick={() => setCaseId(item.id)}
              className={cn(
                "min-h-11 rounded-full border px-3 py-2 font-mono text-[10px] font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#4c3f91] focus-visible:ring-offset-2",
                selected.id === item.id
                  ? "border-[#4c3f91] bg-[#4c3f91] text-white"
                  : "border-[#a9b3ad] bg-white text-[#52615b] hover:border-[#4c3f91] hover:text-[#4c3f91]"
              )}
            >
              {item.label}
            </button>
          ))}
        </div>
      </header>

      <div className="p-5 sm:p-7" aria-live="polite">
        <div className="grid gap-3 lg:grid-cols-[1fr_auto_1fr_auto_1fr] lg:items-stretch">
          <DispatchCard
            icon={Snowflake}
            label="STORAGE CLASS"
            value={selected.storageClass}
          />
          <ArrowRight
            className="mx-auto size-5 rotate-90 self-center text-[#87958e] lg:rotate-0"
            aria-hidden="true"
          />
          <DispatchCard
            icon={ArchiveRestore}
            label="READ POLICY"
            value={selected.policy}
          />
          <ArrowRight
            className="mx-auto size-5 rotate-90 self-center text-[#87958e] lg:rotate-0"
            aria-hidden="true"
          />
          <div
            className={cn(
              "rounded-2xl border-2 p-5",
              RESTORE_TONE[selected.tone]
            )}
          >
            <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] opacity-75">
              APPLICATION RESULT
            </p>
            <p className="m-0 mt-6 font-mono text-xl font-black">
              {selected.result}
            </p>
            <p className="m-0 mt-2 font-mono text-[10px] opacity-80">
              {selected.timing}
            </p>
          </div>
        </div>

        <div className="mt-6 grid gap-4 lg:grid-cols-[1fr_18rem]">
          <div>
            <h4 className="m-0 text-xl font-black">{selected.title}</h4>
            <p className="m-0 mt-2 text-sm leading-6 text-[#52615b]">
              {selected.note}
            </p>
          </div>
          <code className="block overflow-x-auto rounded-xl bg-[#18212a] px-4 py-3 font-mono text-[10px] leading-5 text-[#c7d66d]">
            {selected.command}
          </code>
        </div>
      </div>
    </figure>
  )
}

function DispatchCard({
  icon: Icon,
  label,
  value,
}: {
  icon: typeof Clock3
  label: string
  value: string
}) {
  return (
    <div className="rounded-2xl border border-[#a9b3ad] bg-[#edf2ee] p-5">
      <div className="flex items-center justify-between gap-3">
        <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#60716a]">
          {label}
        </p>
        <Icon className="size-5 text-[#4c3f91]" aria-hidden="true" />
      </div>
      <p className="m-0 mt-7 text-lg font-black">{value}</p>
    </div>
  )
}
