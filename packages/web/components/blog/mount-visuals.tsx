"use client"

import {
  ArrowDown,
  ArrowRight,
  CheckCircle2,
  Cloud,
  FileCheck2,
  FileWarning,
  GitBranch,
  HardDrive,
  Layers3,
  LockKeyhole,
  Network,
  RefreshCcw,
  ServerCog,
  UsersRound,
} from "lucide-react"
import { useState, type ComponentType } from "react"

import { cn } from "@/lib/utils"

type MountStage = {
  label: string
  detail: string
  state: string
  changed?: boolean
}

type MountScene = {
  id: "read" | "write" | "commit" | "retry"
  label: string
  eyebrow: string
  headline: string
  command: string
  note: string
  stages: MountStage[]
}

const MOUNT_SCENES: MountScene[] = [
  {
    id: "read",
    label: "Cold read",
    eyebrow: "01 / RESOLVE",
    headline: "The first read fetches only the ranges the process asks for.",
    command: "head -c 65536 /mnt/vision/models/encoder.safetensors",
    note: "A second read can reuse the verified local range. The Git snapshot never changes.",
    stages: [
      { label: "Snapshot", detail: "commit 4f2c", state: "immutable" },
      { label: "Mounted view", detail: "pointer → recipe", state: "resolve" },
      {
        label: "Local state",
        detail: "+ 64 KiB verified",
        state: "cache",
        changed: true,
      },
      { label: "Publish", detail: "no transaction", state: "idle" },
      { label: "Remote", detail: "xorb ranges", state: "unchanged" },
    ],
  },
  {
    id: "write",
    label: "First write",
    eyebrow: "02 / PROMOTE",
    headline:
      "The first mutation promotes the whole file into the local overlay.",
    command: "model-editor /mnt/vision/models/encoder.safetensors",
    note: "A one-byte edit to a 40 GB file needs room for a coherent 40 GB writable backing file.",
    stages: [
      { label: "Snapshot", detail: "commit 4f2c", state: "immutable" },
      {
        label: "Mounted view",
        detail: "overlay wins",
        state: "write",
        changed: true,
      },
      {
        label: "Local state",
        detail: "40 GB backing",
        state: "dirty",
        changed: true,
      },
      { label: "Publish", detail: "not started", state: "idle" },
      { label: "Remote", detail: "commit 4f2c", state: "unchanged" },
    ],
  },
  {
    id: "commit",
    label: "Commit + push",
    eyebrow: "03 / PUBLISH",
    headline:
      "The barrier freezes a reviewed overlay before Git and Xet move together.",
    command:
      'crab mount commit --mountpoint /mnt/vision -m "Regenerate vision index" --push',
    note: "Xorbs and reconstruction metadata become durable before the remote ref advances.",
    stages: [
      { label: "Snapshot", detail: "base checked", state: "4f2c" },
      { label: "Mounted view", detail: "writers paused", state: "stable" },
      {
        label: "Local state",
        detail: "commit 7ad1",
        state: "recorded",
        changed: true,
      },
      {
        label: "Publish",
        detail: "xet then ref",
        state: "committed",
        changed: true,
      },
      {
        label: "Remote",
        detail: "4f2c → 7ad1",
        state: "advanced",
        changed: true,
      },
    ],
  },
  {
    id: "retry",
    label: "Failure / retry",
    eyebrow: "04 / RECOVER",
    headline:
      "A failed push preserves a transaction you can inspect and retry.",
    command:
      'crab mount commit --mountpoint /mnt/vision -m "Regenerate vision index" --push',
    note: "Do not clean the overlay. Fix credentials or ref state, then retry the recorded publication.",
    stages: [
      { label: "Snapshot", detail: "base retained", state: "4f2c" },
      { label: "Mounted view", detail: "hold writers", state: "review" },
      {
        label: "Local state",
        detail: "commit 7ad1",
        state: "recoverable",
        changed: true,
      },
      {
        label: "Publish",
        detail: "retry record",
        state: "pending",
        changed: true,
      },
      { label: "Remote", detail: "still 4f2c", state: "unchanged" },
    ],
  },
]

const STAGE_ICONS = [GitBranch, Layers3, HardDrive, ServerCog, Cloud]

export function MountLifecycleConsole() {
  const [sceneId, setSceneId] = useState<MountScene["id"]>("read")
  const scene =
    MOUNT_SCENES.find((item) => item.id === sceneId) ?? MOUNT_SCENES[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(68rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.75rem] border border-[#34515b] bg-[#142831] text-[#f8f2e8] shadow-[0_24px_70px_rgba(20,40,49,0.24)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(68rem,calc(100vw-2rem))] lg:w-[min(68rem,calc(100vw-24.5rem))]">
      <header className="grid gap-5 border-b border-white/15 px-5 py-6 sm:px-7 lg:grid-cols-[1fr_auto] lg:items-end">
        <div>
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.22em] text-[#efa66f]">
            MOUNT OPERATIONS CONSOLE / ONE SNAPSHOT / ONE OVERLAY
          </p>
          <h3 className="m-0 mt-2 max-w-2xl text-2xl font-black tracking-[-0.04em] text-white sm:text-3xl">
            Follow one file from cold read to durable remote commit.
          </h3>
        </div>
        <div
          className="flex flex-wrap gap-2"
          aria-label="Mount lifecycle scene"
        >
          {MOUNT_SCENES.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-pressed={scene.id === item.id}
              onClick={() => setSceneId(item.id)}
              className={cn(
                "min-h-11 rounded-full border px-3 py-2 font-mono text-[10px] font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#efa66f] focus-visible:ring-offset-2 focus-visible:ring-offset-[#142831]",
                scene.id === item.id
                  ? "border-[#efa66f] bg-[#efa66f] text-[#142831]"
                  : "border-white/25 bg-white/5 text-white/70 hover:border-white/60 hover:text-white"
              )}
            >
              {item.label}
            </button>
          ))}
        </div>
      </header>

      <div className="grid gap-6 p-5 sm:p-7">
        <div className="grid gap-3 md:grid-cols-5">
          {scene.stages.map((stage, index) => {
            const Icon = STAGE_ICONS[index] as ComponentType<{
              className?: string
            }>
            return (
              <div key={stage.label} className="relative">
                <div
                  className={cn(
                    "h-full min-h-32 rounded-2xl border p-4 transition-colors duration-300 motion-reduce:transition-none",
                    stage.changed
                      ? "border-[#efa66f]/70 bg-[#efa66f]/10"
                      : "border-white/15 bg-white/[0.04]"
                  )}
                >
                  <div className="flex items-center justify-between">
                    <Icon
                      className={cn(
                        "size-5",
                        stage.changed ? "text-[#efa66f]" : "text-[#76aa91]"
                      )}
                    />
                    <span className="font-mono text-[9px] font-black tracking-[0.16em] text-white/45">
                      0{index + 1}
                    </span>
                  </div>
                  <p className="m-0 mt-5 text-xs font-black tracking-[0.08em] text-white/55 uppercase">
                    {stage.label}
                  </p>
                  <p className="m-0 mt-1 text-sm font-bold text-white">
                    {stage.detail}
                  </p>
                  <p
                    className={cn(
                      "m-0 mt-1 font-mono text-[10px]",
                      stage.changed ? "text-[#efa66f]" : "text-[#76aa91]"
                    )}
                  >
                    {stage.state}
                  </p>
                </div>
                {index < scene.stages.length - 1 ? (
                  <ArrowRight className="absolute top-1/2 -right-[18px] z-10 hidden size-5 -translate-y-1/2 text-white/25 md:block" />
                ) : null}
              </div>
            )
          })}
        </div>

        <div className="grid overflow-hidden rounded-2xl border border-white/15 bg-[#0d1d24] lg:grid-cols-[0.72fr_1.28fr]">
          <div className="border-b border-white/15 p-5 lg:border-r lg:border-b-0">
            <p className="m-0 font-mono text-[10px] font-black tracking-[0.18em] text-[#76aa91]">
              {scene.eyebrow}
            </p>
            <p className="m-0 mt-2 text-lg leading-snug font-black text-white">
              {scene.headline}
            </p>
          </div>
          <div className="p-5">
            <div className="flex items-start gap-3 rounded-xl bg-black/25 px-4 py-3">
              <span className="font-mono text-sm text-[#efa66f] select-none">
                $
              </span>
              <code className="font-mono text-xs leading-relaxed break-all text-white/85">
                {scene.command}
              </code>
            </div>
            <p className="m-0 mt-3 text-xs leading-relaxed text-white/60">
              {scene.note}
            </p>
          </div>
        </div>
      </div>
      <figcaption className="border-t border-white/15 px-5 py-3 text-xs leading-relaxed text-white/55 sm:px-7">
        Choose an operation to see which state changes. Orange marks the state
        mutated by that step; the base snapshot remains an explicit reference
        point.
      </figcaption>
    </figure>
  )
}

type WriterMode = {
  id: "paths" | "ranges" | "mounts"
  label: string
  title: string
  description: string
  verdict: string
  verdictDetail: string
  tone: "safe" | "coordinate" | "boundary"
  targets: string[]
}

const WRITER_MODES: WriterMode[] = [
  {
    id: "paths",
    label: "Independent files",
    title: "Four workers, four owned outputs",
    description:
      "Metadata stays consistent while independent backing files make progress in parallel.",
    verdict: "Concurrent by design",
    verdictDetail: "Best fit for shards, partitions, and per-worker artifacts.",
    tone: "safe",
    targets: [
      "shard-000.bin",
      "shard-001.bin",
      "shard-002.bin",
      "shard-003.bin",
    ],
  },
  {
    id: "ranges",
    label: "Same file",
    title: "Four workers, one checkpoint",
    description:
      "Crab serializes the path, but it cannot invent record boundaries or merge application writes.",
    verdict: "Coordinate in the application",
    verdictDetail:
      "Use a single writer, a real file-lock protocol, or atomic replacement.",
    tone: "coordinate",
    targets: ["bytes 0–4K", "bytes 4–8K", "bytes 8–12K", "header + index"],
  },
  {
    id: "mounts",
    label: "Separate mounts",
    title: "Two working trees, one remote ref",
    description:
      "Each mount has its own snapshot and overlay. NFS locks do not cross machines or reconcile Git history.",
    verdict: "Coordinate at the Git boundary",
    verdictDetail:
      "Give jobs separate refs or serialize publication against the remote ref.",
    tone: "boundary",
    targets: ["host-a / main", "host-b / main", "overlay A", "overlay B"],
  },
]

const WRITER_COLORS = ["#315e70", "#d67d45", "#6e9e85", "#816d91"]

export function MountWriterOwnershipLab() {
  const [modeId, setModeId] = useState<WriterMode["id"]>("paths")
  const mode =
    WRITER_MODES.find((item) => item.id === modeId) ?? WRITER_MODES[0]
  const VerdictIcon =
    mode.tone === "safe"
      ? CheckCircle2
      : mode.tone === "coordinate"
        ? LockKeyhole
        : RefreshCcw

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.75rem] border border-[#a6afa8] bg-[#f3f0e7] text-[#192a33] shadow-[0_20px_60px_rgba(25,42,51,0.13)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="grid gap-5 border-b border-[#b9b7ac] px-5 py-6 sm:px-7 lg:grid-cols-[1fr_auto] lg:items-end">
        <div>
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-[#315e70]">
            WRITER OWNERSHIP LAB / LOCAL NFS CLIENT
          </p>
          <h3 className="m-0 mt-2 text-2xl font-black tracking-[-0.04em] sm:text-3xl">
            Concurrency follows ownership, not writer count.
          </h3>
        </div>
        <div
          className="flex flex-wrap gap-2"
          aria-label="Writer ownership scenario"
        >
          {WRITER_MODES.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-pressed={mode.id === item.id}
              onClick={() => setModeId(item.id)}
              className={cn(
                "min-h-11 rounded-full border px-3 py-2 font-mono text-[10px] font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#315e70] focus-visible:ring-offset-2 focus-visible:ring-offset-[#f3f0e7]",
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

      <div className="grid gap-5 p-5 sm:p-7 lg:grid-cols-[1.35fr_0.65fr]">
        <div className="rounded-2xl border border-[#c7c5ba] bg-white/55 p-4 sm:p-5">
          <div className="flex items-start gap-3">
            <UsersRound className="mt-0.5 size-5 shrink-0 text-[#315e70]" />
            <div>
              <p className="m-0 text-base font-black">{mode.title}</p>
              <p className="m-0 mt-1 text-xs leading-relaxed text-[#5d696c]">
                {mode.description}
              </p>
            </div>
          </div>
          <div className="mt-5 grid gap-2">
            {mode.targets.map((target, index) => (
              <div
                key={target}
                className="grid grid-cols-[5.5rem_1fr] items-center gap-2 sm:grid-cols-[7rem_1fr]"
              >
                <div className="flex min-h-11 items-center gap-2 rounded-xl border border-[#d0cec3] bg-[#f3f0e7] px-3">
                  <span
                    className="size-2.5 rounded-full"
                    style={{ backgroundColor: WRITER_COLORS[index] }}
                  />
                  <span className="font-mono text-[10px] font-black">
                    worker {index + 1}
                  </span>
                </div>
                <div className="flex min-h-11 items-center gap-2 overflow-hidden rounded-xl border border-[#d0cec3] bg-white px-3">
                  <ArrowRight className="size-4 shrink-0 text-[#8c9694]" />
                  <span className="truncate font-mono text-[10px] font-bold text-[#45565c]">
                    {target}
                  </span>
                </div>
              </div>
            ))}
          </div>
        </div>

        <div
          className={cn(
            "flex min-h-52 flex-col justify-between rounded-2xl border p-5",
            mode.tone === "safe" && "border-[#6e9e85] bg-[#dfe9df]",
            mode.tone === "coordinate" && "border-[#d67d45] bg-[#f3dfcd]",
            mode.tone === "boundary" && "border-[#816d91] bg-[#e7e0e9]"
          )}
        >
          <div>
            <VerdictIcon className="size-7" />
            <p className="m-0 mt-5 font-mono text-[10px] font-black tracking-[0.16em]">
              VERDICT
            </p>
            <p className="m-0 mt-2 text-xl leading-tight font-black">
              {mode.verdict}
            </p>
            <p className="m-0 mt-3 text-xs leading-relaxed text-[#4d5c60]">
              {mode.verdictDetail}
            </p>
          </div>
          <div className="mt-6 flex items-center gap-2 border-t border-current/15 pt-4 font-mono text-[9px] font-black tracking-[0.12em]">
            {mode.id === "mounts" ? (
              <Network className="size-4" />
            ) : mode.id === "ranges" ? (
              <FileWarning className="size-4" />
            ) : (
              <FileCheck2 className="size-4" />
            )}
            {mode.id === "paths"
              ? "PATH OWNERSHIP IS CLEAR"
              : mode.id === "ranges"
                ? "BYTES HAVE SHARED OWNERSHIP"
                : "REF HAS SHARED OWNERSHIP"}
          </div>
        </div>
      </div>
      <figcaption className="flex items-start gap-2 border-t border-[#b9b7ac] px-5 py-3 text-xs leading-relaxed text-[#5d696c] sm:px-7">
        <ArrowDown className="mt-0.5 size-4 shrink-0" />
        Switch scenarios to identify where coordination belongs: the output
        path, the application record, or the Git ref.
      </figcaption>
    </figure>
  )
}
