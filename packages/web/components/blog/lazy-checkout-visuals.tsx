"use client"

import {
  ArrowRight,
  Box,
  Cloud,
  Database,
  FileArchive,
  FileCode2,
  FolderTree,
  HardDrive,
  Laptop,
  MousePointerClick,
  PackageCheck,
  PencilLine,
  ScanSearch,
} from "lucide-react"
import { useState } from "react"

import { cn } from "@/lib/utils"

type MaterializationMode = {
  id: "pointers" | "hydrate" | "mount"
  label: string
  local: string
  headline: string
  bestFor: string
  command: string
  packed: { label: string; size: string; color: string }[]
  remote: { label: string; size: string }[]
}

const MATERIALIZATION_MODES: MaterializationMode[] = [
  {
    id: "pointers",
    label: "Pointers",
    local: "180 MB",
    headline: "Carry identity, not payloads",
    bestFor: "Review code, inspect branches, run metadata-only jobs",
    command: "crab clone crab://team-data/vision-search",
    packed: [
      { label: "Git + source", size: "120 MB", color: "bg-[#2f6478]" },
      { label: "Pointers", size: "60 MB", color: "bg-[#e18c4f]" },
    ],
    remote: [
      { label: "Models", size: "820 GB" },
      { label: "Datasets", size: "3.1 TB" },
    ],
  },
  {
    id: "hydrate",
    label: "Hydrate set",
    local: "84 GB",
    headline: "Pack the known working set",
    bestFor: "Training, CI, or any job with a stable input list",
    command: "crab hydrate --manifest-ref HEAD:.crab/manifests/training.txt",
    packed: [
      { label: "Git + pointers", size: "180 MB", color: "bg-[#2f6478]" },
      { label: "Model", size: "12 GB", color: "bg-[#e18c4f]" },
      { label: "Training data", size: "72 GB", color: "bg-[#67a58b]" },
    ],
    remote: [
      { label: "Other models", size: "808 GB" },
      { label: "Other data", size: "3.0 TB" },
    ],
  },
  {
    id: "mount",
    label: "Mount",
    local: "9 GB →",
    headline: "Fetch as the application explores",
    bestFor: "Asset browsers, analysis tools, and unpredictable reads",
    command:
      "crab mount -r crab://team-data/vision-search -m /mnt/vision --read-only",
    packed: [
      { label: "Snapshot", size: "180 MB", color: "bg-[#2f6478]" },
      { label: "Read cache", size: "grows", color: "bg-[#e18c4f]" },
    ],
    remote: [
      { label: "Unread content", size: "stays remote" },
      { label: "Read ranges", size: "arrive on demand" },
    ],
  },
]

export function MaterializationPackingDesk() {
  const [modeId, setModeId] = useState<MaterializationMode["id"]>("pointers")
  const mode =
    MATERIALIZATION_MODES.find((item) => item.id === modeId) ??
    MATERIALIZATION_MODES[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.75rem] border border-[#a6afa8] bg-[#f3f0e7] text-[#192a33] shadow-[0_20px_60px_rgba(25,42,51,0.13)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="grid gap-5 border-b border-[#b9b7ac] px-5 py-5 sm:px-7 lg:grid-cols-[1fr_auto] lg:items-end">
        <div>
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-[#2f6478]">
            MATERIALIZATION DESK / 4 TB REPO / 256 GB LAPTOP
          </p>
          <h3 className="m-0 mt-2 text-2xl font-black tracking-[-0.04em] sm:text-3xl">
            What actually goes in the laptop?
          </h3>
        </div>
        <div className="flex flex-wrap gap-2" aria-label="Materialization mode">
          {MATERIALIZATION_MODES.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-pressed={mode.id === item.id}
              onClick={() => setModeId(item.id)}
              className={cn(
                "min-h-11 rounded-full border px-3 py-1.5 font-mono text-[10px] font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#2f6478] focus-visible:ring-offset-2 focus-visible:ring-offset-[#f3f0e7]",
                mode.id === item.id
                  ? "border-[#192a33] bg-[#192a33] text-white"
                  : "border-[#9ba39c] bg-white/60 text-[#53626a] hover:border-[#192a33] hover:text-[#192a33]"
              )}
            >
              {item.label}
            </button>
          ))}
        </div>
      </header>

      <div className="grid lg:grid-cols-[1fr_1.15fr]">
        <section className="border-b border-[#b9b7ac] p-5 sm:p-7 lg:border-r lg:border-b-0">
          <div className="flex items-center gap-3">
            <Cloud className="size-6 text-[#2f6478]" aria-hidden="true" />
            <div>
              <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#708087]">
                OBJECT STORAGE
              </p>
              <p className="m-0 text-lg font-black">The 4 TB source of truth</p>
            </div>
          </div>
          <div className="mt-5 grid gap-3 sm:grid-cols-2 lg:grid-cols-1 xl:grid-cols-2">
            {mode.remote.map((item) => (
              <div
                key={item.label}
                className="rounded-2xl border border-dashed border-[#8ea0a6] bg-[#dce8e8] p-4"
              >
                <FileArchive
                  className="size-5 text-[#2f6478]"
                  aria-hidden="true"
                />
                <p className="m-0 mt-5 text-sm font-black">{item.label}</p>
                <p className="m-0 mt-1 font-mono text-xs text-[#53626a]">
                  {item.size}
                </p>
              </div>
            ))}
          </div>
          <div className="mt-5 flex items-center gap-3 font-mono text-[10px] font-bold text-[#53626a]">
            <span className="h-px flex-1 border-t border-dashed border-[#8ea0a6]" />
            {mode.id === "mount"
              ? "RANGES CROSS WHEN READ"
              : "ONLY THE SELECTION CROSSES"}
            <ArrowRight className="size-4" aria-hidden="true" />
          </div>
        </section>

        <section className="bg-[#fbfaf5] p-5 sm:p-7" aria-live="polite">
          <div className="flex items-start justify-between gap-4">
            <div>
              <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#708087]">
                LOCAL DISK / 256 GB
              </p>
              <h4 className="m-0 mt-1 text-xl font-black">{mode.headline}</h4>
            </div>
            <div className="shrink-0 rounded-xl bg-[#192a33] px-3 py-2 text-right text-white">
              <p className="m-0 font-mono text-[9px] text-[#bdc8cb]">
                STARTS AT
              </p>
              <p className="m-0 font-mono text-xl font-black">{mode.local}</p>
            </div>
          </div>

          <div className="relative mt-6 overflow-hidden rounded-[1.35rem] border-2 border-[#192a33] bg-[#e8e1d2] p-3 pt-7">
            <div className="absolute top-0 left-1/2 h-3 w-20 -translate-x-1/2 rounded-b-lg border-x-2 border-b-2 border-[#192a33] bg-[#fbfaf5]" />
            <div className="grid min-h-32 gap-2 sm:grid-cols-3">
              {mode.packed.map((item) => (
                <div
                  key={item.label}
                  className={cn(
                    "flex min-h-24 flex-col justify-between rounded-xl p-3 text-white shadow-[inset_0_0_0_1px_rgba(255,255,255,0.25)]",
                    item.color
                  )}
                >
                  <Box className="size-4" aria-hidden="true" />
                  <div>
                    <p className="m-0 text-xs font-black">{item.label}</p>
                    <p className="m-0 mt-1 font-mono text-[10px] text-white/80">
                      {item.size}
                    </p>
                  </div>
                </div>
              ))}
              <div className="flex min-h-24 items-center justify-center rounded-xl border border-dashed border-[#9f998e] font-mono text-[10px] font-black text-[#777166]">
                HEADROOM
              </div>
            </div>
          </div>

          <div className="mt-5 grid gap-3 sm:grid-cols-[1fr_auto] sm:items-center">
            <p className="m-0 text-sm leading-6 text-[#53626a]">
              <span className="font-black text-[#192a33]">Best for:</span>{" "}
              {mode.bestFor}
            </p>
            <Laptop
              className="hidden size-6 text-[#e18c4f] sm:block"
              aria-hidden="true"
            />
          </div>
          <code className="mt-4 block overflow-x-auto rounded-xl bg-[#192a33] px-4 py-3 font-mono text-[11px] text-[#f8d6b9]">
            {mode.command}
          </code>
        </section>
      </div>
      <figcaption className="border-t border-[#b9b7ac] px-5 py-3 font-mono text-[10px] text-[#657279] sm:px-7">
        Illustrative sizes. The committed file identities stay the same in every
        mode.
      </figcaption>
    </figure>
  )
}

type ReadScene = {
  id: "list" | "cold" | "warm" | "write"
  label: string
  title: string
  note: string
  before: string
  after: string
  structures: string[]
  steps: {
    icon: typeof FolderTree
    label: string
    detail: string
    active?: boolean
  }[]
}

const READ_SCENES: ReadScene[] = [
  {
    id: "list",
    label: "List folder",
    title: "Names arrive without large-file bytes",
    note: "The resolver merges the base snapshot with overlay entries, then returns directory names.",
    before: "cache 0 MB",
    after: "cache 0 MB",
    structures: ["snapshot row", "overlay entry", "directory page"],
    steps: [
      { icon: FolderTree, label: "readdir()", detail: "models/" },
      {
        icon: Database,
        label: "Snapshot + overlay",
        detail: "merge children",
        active: true,
      },
      { icon: Laptop, label: "Application", detail: "names + types" },
    ],
  },
  {
    id: "cold",
    label: "First read",
    title: "Only the requested window is reconstructed",
    note: "A pointer maps the byte range to verified remote content. The completed window is cached locally.",
    before: "cache 0 MB",
    after: "cache 8 MB",
    structures: ["pointer", "file recipe", "xorb range", "read window"],
    steps: [
      {
        icon: MousePointerClick,
        label: "read(64 KiB)",
        detail: "offset 32 MiB",
      },
      { icon: FileCode2, label: "Pointer", detail: "hash + size" },
      {
        icon: Cloud,
        label: "Xorb ranges",
        detail: "verified bytes",
        active: true,
      },
      { icon: HardDrive, label: "Read cache", detail: "8 MiB window" },
      { icon: Laptop, label: "Application", detail: "exact 64 KiB" },
    ],
  },
  {
    id: "warm",
    label: "Repeat read",
    title: "The request stops at the local window",
    note: "The same file hash and window key find complete cached bytes, so object storage is not contacted.",
    before: "cache 8 MB",
    after: "network 0 B",
    structures: ["file hash", "window key", "verified cache file"],
    steps: [
      { icon: MousePointerClick, label: "read(64 KiB)", detail: "same offset" },
      { icon: FileCode2, label: "Pointer", detail: "same identity" },
      {
        icon: HardDrive,
        label: "Read cache",
        detail: "complete hit",
        active: true,
      },
      { icon: Laptop, label: "Application", detail: "exact 64 KiB" },
    ],
  },
  {
    id: "write",
    label: "First write",
    title: "The base file is promoted into the overlay",
    note: "A writable mount reconstructs the full base file into local overlay backing before applying the edit.",
    before: "overlay 0 GB",
    after: "overlay +12 GB",
    structures: [
      "base pointer",
      "temp backing file",
      "overlay row",
      "source OID",
    ],
    steps: [
      { icon: PencilLine, label: "write()", detail: "writable mount" },
      { icon: FileCode2, label: "Base pointer", detail: "12 GB file" },
      {
        icon: PackageCheck,
        label: "Full promotion",
        detail: "temp → atomic rename",
        active: true,
      },
      { icon: HardDrive, label: "Overlay backing", detail: "local copy" },
      { icon: Laptop, label: "Application", detail: "read-after-write" },
    ],
  },
]

export function MountedReadCutaway() {
  const [sceneId, setSceneId] = useState<ReadScene["id"]>("cold")
  const scene =
    READ_SCENES.find((item) => item.id === sceneId) ?? READ_SCENES[1]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.75rem] bg-[#172830] text-[#f7f3e9] shadow-[0_20px_60px_rgba(23,40,48,0.2)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="border-b border-[#49606a] px-5 py-5 sm:px-7">
        <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-[#efad72]">
          MOUNT CUTAWAY / SELECT AN OS OPERATION
        </p>
        <div
          className="mt-3 flex flex-wrap gap-2"
          aria-label="Filesystem operation"
        >
          {READ_SCENES.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-pressed={scene.id === item.id}
              onClick={() => setSceneId(item.id)}
              className={cn(
                "min-h-11 rounded-lg border px-3 py-2 font-mono text-[10px] font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#efad72] focus-visible:ring-offset-2 focus-visible:ring-offset-[#172830]",
                scene.id === item.id
                  ? "border-[#efad72] bg-[#efad72] text-[#172830]"
                  : "border-[#5d737c] text-[#b9c8cd] hover:border-[#efad72] hover:text-white"
              )}
            >
              {item.label}
            </button>
          ))}
        </div>
      </header>

      <div className="p-5 sm:p-7" aria-live="polite">
        <div className="grid gap-5 lg:grid-cols-[1fr_auto] lg:items-start">
          <div>
            <h3 className="m-0 text-2xl font-black tracking-[-0.035em] sm:text-3xl">
              {scene.title}
            </h3>
            <p className="m-0 mt-2 max-w-2xl text-sm leading-6 text-[#b9c8cd]">
              {scene.note}
            </p>
          </div>
          <div className="grid grid-cols-[1fr_auto_1fr] items-center gap-3 rounded-xl border border-[#49606a] bg-[#203841] px-4 py-3 font-mono text-[10px] font-black">
            <span className="text-[#b9c8cd]">{scene.before}</span>
            <ArrowRight className="size-4 text-[#efad72]" aria-hidden="true" />
            <span className="text-right text-white">{scene.after}</span>
          </div>
        </div>

        <div className="mt-7 flex flex-col gap-2 lg:flex-row lg:items-stretch">
          {scene.steps.map((step, index) => {
            const Icon = step.icon
            return (
              <div key={`${scene.id}-${step.label}`} className="contents">
                <div
                  className={cn(
                    "min-w-0 flex-1 rounded-2xl border p-4 transition-colors duration-300 motion-reduce:transition-none",
                    step.active
                      ? "border-[#efad72] bg-[#5a392b] shadow-[inset_0_0_0_1px_#efad72]"
                      : "border-[#49606a] bg-[#203841]"
                  )}
                >
                  <div className="flex items-center justify-between gap-3">
                    <Icon
                      className="size-5 text-[#efad72]"
                      aria-hidden="true"
                    />
                    <span className="font-mono text-[9px] font-black text-[#78909a]">
                      {String(index + 1).padStart(2, "0")}
                    </span>
                  </div>
                  <p className="m-0 mt-5 text-sm font-black text-white">
                    {step.label}
                  </p>
                  <p className="m-0 mt-1 font-mono text-[9px] leading-4 text-[#b9c8cd]">
                    {step.detail}
                  </p>
                </div>
                {index < scene.steps.length - 1 ? (
                  <div className="flex items-center justify-center text-[#78909a]">
                    <ArrowRight
                      className="size-4 rotate-90 lg:rotate-0"
                      aria-hidden="true"
                    />
                  </div>
                ) : null}
              </div>
            )
          })}
        </div>

        <div className="mt-6 border-t border-dashed border-[#5d737c] pt-4">
          <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#78909a]">
            KEY DATA STRUCTURES
          </p>
          <div className="mt-3 flex flex-wrap gap-2">
            {scene.structures.map((structure) => (
              <span
                key={structure}
                className="rounded-full border border-[#5d737c] bg-[#203841] px-3 py-1.5 font-mono text-[10px] text-[#d6e0e2]"
              >
                {structure}
              </span>
            ))}
          </div>
        </div>
      </div>
    </figure>
  )
}

type BudgetScenario = {
  id: "browse" | "training" | "writable" | "overflow"
  label: string
  status: "roomy" | "tight" | "over"
  advice: string
  parts: { label: string; value: number; color: string }[]
}

const BUDGET_SCENARIOS: BudgetScenario[] = [
  {
    id: "browse",
    label: "Browse assets",
    status: "roomy",
    advice:
      "A read-only mount leaves 150 GB for new windows and normal laptop use.",
    parts: [
      { label: "Hydrated", value: 0, color: "bg-[#2f6478]" },
      { label: "Cache", value: 18, color: "bg-[#e18c4f]" },
      { label: "Overlay", value: 0, color: "bg-[#a66b7b]" },
      { label: "App scratch", value: 24, color: "bg-[#67a58b]" },
      { label: "System reserve", value: 64, color: "bg-[#7d8588]" },
    ],
  },
  {
    id: "training",
    label: "Train model",
    status: "tight",
    advice:
      "Only 16 GB remains. Narrow the manifest or reduce scratch/cache before starting.",
    parts: [
      { label: "Hydrated", value: 96, color: "bg-[#2f6478]" },
      { label: "Cache", value: 32, color: "bg-[#e18c4f]" },
      { label: "Overlay", value: 0, color: "bg-[#a66b7b]" },
      { label: "App scratch", value: 48, color: "bg-[#67a58b]" },
      { label: "System reserve", value: 64, color: "bg-[#7d8588]" },
    ],
  },
  {
    id: "writable",
    label: "Edit 118 GB file",
    status: "tight",
    advice:
      "First write promotes the full file. Export or commit the overlay before it crowds the disk.",
    parts: [
      { label: "Hydrated", value: 0, color: "bg-[#2f6478]" },
      { label: "Cache", value: 22, color: "bg-[#e18c4f]" },
      { label: "Overlay", value: 118, color: "bg-[#a66b7b]" },
      { label: "App scratch", value: 24, color: "bg-[#67a58b]" },
      { label: "System reserve", value: 64, color: "bg-[#7d8588]" },
    ],
  },
  {
    id: "overflow",
    label: "Hydrate too much",
    status: "over",
    advice:
      "This plan exceeds the disk by 80 GB. A broad glob is not a safe working set.",
    parts: [
      { label: "Hydrated", value: 160, color: "bg-[#2f6478]" },
      { label: "Cache", value: 48, color: "bg-[#e18c4f]" },
      { label: "Overlay", value: 0, color: "bg-[#a66b7b]" },
      { label: "App scratch", value: 64, color: "bg-[#67a58b]" },
      { label: "System reserve", value: 64, color: "bg-[#7d8588]" },
    ],
  },
]

export function DiskBudgetWorkbench() {
  const [scenarioId, setScenarioId] = useState<BudgetScenario["id"]>("browse")
  const scenario =
    BUDGET_SCENARIOS.find((item) => item.id === scenarioId) ??
    BUDGET_SCENARIOS[0]
  const used = scenario.parts.reduce((total, part) => total + part.value, 0)
  const free = 256 - used

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.75rem] border border-[#b9b7ac] bg-[#fbfaf5] text-[#192a33] shadow-[0_20px_60px_rgba(25,42,51,0.12)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="grid gap-5 border-b border-[#b9b7ac] bg-[#f3f0e7] px-5 py-5 sm:px-7 lg:grid-cols-[1fr_auto] lg:items-end">
        <div>
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-[#2f6478]">
            CAPACITY BENCH / 256 GB DISK
          </p>
          <h3 className="m-0 mt-2 text-2xl font-black tracking-[-0.04em] sm:text-3xl">
            Four stores compete for the same space
          </h3>
        </div>
        <ScanSearch
          className="hidden size-8 text-[#e18c4f] lg:block"
          aria-hidden="true"
        />
      </header>

      <div className="grid lg:grid-cols-[17rem_1fr]">
        <div className="border-b border-[#b9b7ac] p-5 sm:p-7 lg:border-r lg:border-b-0">
          <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#708087]">
            TRY A WORKLOAD
          </p>
          <div className="mt-3 grid gap-2 sm:grid-cols-2 lg:grid-cols-1">
            {BUDGET_SCENARIOS.map((item) => (
              <button
                key={item.id}
                type="button"
                aria-pressed={scenario.id === item.id}
                onClick={() => setScenarioId(item.id)}
                className={cn(
                  "rounded-xl border px-4 py-3 text-left text-sm font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#2f6478] focus-visible:ring-offset-2",
                  scenario.id === item.id
                    ? "border-[#192a33] bg-[#192a33] text-white"
                    : "border-[#c6c4ba] bg-white text-[#53626a] hover:border-[#192a33] hover:text-[#192a33]"
                )}
              >
                {item.label}
              </button>
            ))}
          </div>
        </div>

        <div className="p-5 sm:p-7" aria-live="polite">
          <div className="flex flex-wrap items-end justify-between gap-4">
            <div>
              <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#708087]">
                PLANNED LOCAL FOOTPRINT
              </p>
              <p className="m-0 mt-1 font-mono text-4xl font-black tracking-[-0.07em]">
                {used} GB
              </p>
            </div>
            <div
              className={cn(
                "rounded-full px-3 py-1.5 font-mono text-[10px] font-black",
                scenario.status === "roomy" && "bg-[#dcecdf] text-[#24563b]",
                scenario.status === "tight" && "bg-[#fae0bd] text-[#73451e]",
                scenario.status === "over" && "bg-[#f7c9c2] text-[#7c2f2a]"
              )}
            >
              {free >= 0 ? `${free} GB FREE` : `${Math.abs(free)} GB OVER`}
            </div>
          </div>

          <div className="mt-6 overflow-hidden rounded-xl border-2 border-[#192a33] bg-[#e8e1d2] p-1">
            <div
              className="flex h-16 min-w-full gap-1"
              style={{ width: `${Math.max(100, (used / 256) * 100)}%` }}
            >
              {scenario.parts
                .filter((part) => part.value > 0)
                .map((part) => (
                  <div
                    key={part.label}
                    className={cn("min-w-2 rounded-md", part.color)}
                    style={{ width: `${(part.value / used) * 100}%` }}
                    title={`${part.label}: ${part.value} GB`}
                  />
                ))}
            </div>
          </div>
          <div className="mt-2 flex justify-between font-mono text-[9px] font-bold text-[#708087]">
            <span>0 GB</span>
            <span>DISK LIMIT 256 GB</span>
          </div>

          <div className="mt-6 grid gap-3 sm:grid-cols-2 xl:grid-cols-5">
            {scenario.parts.map((part) => (
              <div
                key={part.label}
                className="rounded-xl border border-[#d0cdc1] bg-[#f3f0e7] p-3"
              >
                <div className={cn("h-2 w-8 rounded-full", part.color)} />
                <p className="m-0 mt-3 text-xs font-black">{part.label}</p>
                <p className="m-0 mt-1 font-mono text-[10px] text-[#708087]">
                  {part.value} GB
                </p>
              </div>
            ))}
          </div>

          <div className="mt-6 flex gap-3 rounded-xl border border-[#d0cdc1] bg-[#f3f0e7] p-4">
            <HardDrive
              className="mt-0.5 size-5 shrink-0 text-[#e18c4f]"
              aria-hidden="true"
            />
            <p className="m-0 text-sm leading-6 text-[#53626a]">
              {scenario.advice}
            </p>
          </div>
        </div>
      </div>
      <figcaption className="border-t border-[#b9b7ac] px-5 py-3 font-mono text-[10px] text-[#657279] sm:px-7">
        Example budgets include a 64 GB reserve for the OS and ordinary work.
      </figcaption>
    </figure>
  )
}
