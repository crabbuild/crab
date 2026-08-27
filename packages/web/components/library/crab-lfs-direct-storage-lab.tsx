"use client"

import {
  ArrowRight,
  Cloud,
  Download,
  FileCode2,
  GitBranch,
  HardDrive,
  Laptop,
  ServerOff,
  Terminal,
  Upload,
} from "lucide-react"
import { useState } from "react"
import type { LucideIcon } from "lucide-react"

import { cn } from "@/lib/utils"

type LfsStageId = "add" | "push" | "pull"
type SurfaceTone = "git" | "local" | "remote" | "quiet"

interface SurfaceItem {
  label: string
  value: string
  tone: SurfaceTone
}

interface LfsStage {
  id: LfsStageId
  label: string
  title: string
  command: string
  description: string
  invariant: string
  local: SurfaceItem[]
  remote: SurfaceItem[]
  icon: LucideIcon
}

const STAGES: LfsStage[] = [
  {
    id: "add",
    label: "1 · Add",
    title: "Git receives a pointer, not the 8 GB file",
    command: "git add models/encoder.safetensors",
    description:
      "Crab's LFS clean filter keeps the full file in your worktree, writes a standard LFS pointer into Git, and saves the bytes in the local LFS cache.",
    invariant: "Git history stays small; the file identity stays exact.",
    icon: FileCode2,
    local: [
      { label: "Worktree", value: "full file bytes", tone: "local" },
      { label: "Git index", value: "LFS pointer · ~130 B", tone: "git" },
      { label: "Local cache", value: ".git/lfs/objects/<oid>", tone: "local" },
    ],
    remote: [
      { label: "Bucket", value: "no network write yet", tone: "quiet" },
      { label: "Gateway", value: "not required", tone: "quiet" },
    ],
  },
  {
    id: "push",
    label: "2 · Push",
    title: "The local Crab client writes directly to the bucket",
    command: "git push origin main",
    description:
      "The Crab pre-push hook or Git LFS custom transfer agent uploads the missing LFS object directly to the configured object store before the Git ref advances.",
    invariant: "The ref cannot expose a pointer before its object is durable.",
    icon: Upload,
    local: [
      { label: "Git ref", value: "commit + pointer", tone: "git" },
      { label: "Transfer", value: "Crab runs locally", tone: "local" },
      { label: "Credentials", value: "your provider chain", tone: "local" },
    ],
    remote: [
      { label: "LFS object", value: "lfs/objects/<oid>", tone: "remote" },
      { label: "Shared state", value: "bucket + ref", tone: "remote" },
    ],
  },
  {
    id: "pull",
    label: "3 · Pull",
    title: "A fresh client brings back verified bytes",
    command: "git lfs pull origin main",
    description:
      "A collaborator can use standard Git LFS commands, or crab lfs pull, to fetch the object from the bucket and replace the pointer in the working tree.",
    invariant:
      "The downloaded bytes must match the pointer's SHA-256 identity.",
    icon: Download,
    local: [
      { label: "Git checkout", value: "pointer first", tone: "git" },
      { label: "Local cache", value: "verified object", tone: "local" },
      { label: "Worktree", value: "materialized bytes", tone: "local" },
    ],
    remote: [
      { label: "Read", value: "direct object-store GET", tone: "remote" },
      { label: "No middle tier", value: "no Crab server", tone: "quiet" },
    ],
  },
]

const TONE_STYLES: Record<
  SurfaceTone,
  { border: string; background: string; label: string; value: string }
> = {
  git: {
    border: "border-[#f0a08f]",
    background: "bg-[#fff0eb]",
    label: "text-[#9b463a]",
    value: "text-[#6d2921]",
  },
  local: {
    border: "border-[#8fc8dd]",
    background: "bg-[#eaf7fb]",
    label: "text-[#27718b]",
    value: "text-[#164c60]",
  },
  remote: {
    border: "border-[#9bb3ec]",
    background: "bg-[#eef2ff]",
    label: "text-[#4966a6]",
    value: "text-[#293d73]",
  },
  quiet: {
    border: "border-dashed border-[#b5bfca]",
    background: "bg-[#f7f8fa]",
    label: "text-[#68788c]",
    value: "text-[#526174]",
  },
}

/**
 * Shows the direct-storage Crab LFS route at each user-visible boundary.
 */
export function CrabLfsDirectStorageLab() {
  const [stageId, setStageId] = useState<LfsStageId>("add")
  const stage =
    STAGES.find((candidate) => candidate.id === stageId) ?? STAGES[0]
  const StageIcon = stage.icon

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.5rem] border-2 border-[#17233b] bg-[#eef1f4] text-[#17233b] shadow-[0_24px_70px_rgba(23,35,59,0.18)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="border-b border-[#aab5c3] px-5 py-5 sm:px-7">
        <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-[#49617d]">
          DIRECT STORAGE LAB / CLICK A BOUNDARY
        </p>
        <div className="mt-3 flex flex-col gap-5 lg:flex-row lg:items-end lg:justify-between">
          <div>
            <h3 className="m-0 text-2xl font-black tracking-[-0.04em] sm:text-3xl">
              One LFS file. No gateway.
            </h3>
            <p className="m-0 mt-2 max-w-2xl text-sm leading-6 text-[#52637a]">
              Crab runs on the developer machine and talks to the bucket. The
              Git LFS pointer remains standard so existing tooling can keep
              reading it.
            </p>
          </div>
          <div
            className="flex flex-wrap gap-2"
            role="tablist"
            aria-label="Crab LFS direct-storage stages"
          >
            {STAGES.map((candidate) => (
              <button
                key={candidate.id}
                type="button"
                role="tab"
                aria-selected={stageId === candidate.id}
                onClick={() => setStageId(candidate.id)}
                className={cn(
                  "min-h-11 rounded-lg border px-3 py-2 font-mono text-[10px] font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#e56b5d] focus-visible:ring-offset-2 focus-visible:ring-offset-[#eef1f4]",
                  stageId === candidate.id
                    ? "border-[#17233b] bg-[#17233b] text-white"
                    : "border-[#9aa8b9] bg-white text-[#49617d] hover:border-[#17233b] hover:text-[#17233b]"
                )}
              >
                {candidate.label}
              </button>
            ))}
          </div>
        </div>
      </header>

      <div
        className="grid lg:grid-cols-[minmax(0,1fr)_18rem]"
        aria-live="polite"
      >
        <section className="border-b border-[#aab5c3] p-5 sm:p-7 lg:border-r lg:border-b-0">
          <div className="flex items-center gap-2">
            <StageIcon className="size-5 text-[#e56b5d]" aria-hidden="true" />
            <h4 className="m-0 text-lg font-black">{stage.title}</h4>
          </div>

          <div className="mt-5 grid gap-3 md:grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)] md:items-stretch">
            <SurfaceCard
              icon={Laptop}
              title="Your machine"
              items={stage.local}
            />
            <ArrowRight
              className="mx-auto size-5 rotate-90 self-center text-[#7d8da4] md:rotate-0"
              aria-hidden="true"
            />
            <SurfaceCard
              icon={Cloud}
              title="Your bucket"
              items={stage.remote}
            />
          </div>

          <div className="mt-4 rounded-2xl bg-[#17233b] p-4 text-white sm:p-5">
            <div className="flex items-center gap-2 font-mono text-[9px] font-black tracking-[0.16em] text-[#9db1ca]">
              <Terminal
                className="size-3.5 text-[#39a9db]"
                aria-hidden="true"
              />
              RUNS ON YOUR MACHINE
            </div>
            <code className="mt-3 block overflow-x-auto font-mono text-[11px] leading-6 whitespace-pre-wrap text-[#dce7f3]">
              $ {stage.command}
            </code>
            <p className="m-0 mt-3 text-xs leading-5 text-[#b9c9da]">
              {stage.description}
            </p>
          </div>
        </section>

        <aside className="bg-white p-5 sm:p-7">
          <ServerOff className="size-7 text-[#e56b5d]" aria-hidden="true" />
          <p className="m-0 mt-5 font-mono text-[9px] font-black tracking-[0.16em] text-[#61738a]">
            DELIBERATELY ABSENT
          </p>
          <p className="m-0 mt-2 text-lg leading-6 font-black">
            No Crab LFS server to deploy.
          </p>
          <ul className="m-0 mt-5 space-y-3 text-sm leading-5 text-[#52637a]">
            <li className="flex gap-2">
              <GitBranch
                className="mt-0.5 size-4 shrink-0 text-[#e56b5d]"
                aria-hidden="true"
              />
              Git keeps commits and the standard LFS pointer.
            </li>
            <li className="flex gap-2">
              <HardDrive
                className="mt-0.5 size-4 shrink-0 text-[#39a9db]"
                aria-hidden="true"
              />
              The bucket keeps immutable LFS objects.
            </li>
            <li className="flex gap-2">
              <Cloud
                className="mt-0.5 size-4 shrink-0 text-[#4966a6]"
                aria-hidden="true"
              />
              Your credentials go directly to object storage.
            </li>
          </ul>
          <div className="mt-6 border-t border-dashed border-[#aab5c3] pt-4">
            <p className="m-0 font-mono text-[9px] font-black tracking-[0.14em] text-[#61738a]">
              PROTECTED IN THIS STEP
            </p>
            <p className="m-0 mt-2 text-sm leading-6 font-bold text-[#245c3a]">
              {stage.invariant}
            </p>
          </div>
        </aside>
      </div>
    </figure>
  )
}

function SurfaceCard({
  icon: Icon,
  title,
  items,
}: {
  icon: LucideIcon
  title: string
  items: SurfaceItem[]
}) {
  return (
    <div className="rounded-2xl border-2 border-[#17233b] bg-white p-4">
      <div className="flex items-center gap-2">
        <Icon className="size-4 text-[#49617d]" aria-hidden="true" />
        <p className="m-0 font-mono text-[10px] font-black tracking-[0.14em] text-[#49617d]">
          {title.toUpperCase()}
        </p>
      </div>
      <div className="mt-4 space-y-2">
        {items.map((item) => {
          const style = TONE_STYLES[item.tone]
          return (
            <div
              key={`${item.label}-${item.value}`}
              className={cn(
                "rounded-xl border p-3",
                style.border,
                style.background
              )}
            >
              <p
                className={cn(
                  "m-0 font-mono text-[9px] font-black tracking-[0.12em]",
                  style.label
                )}
              >
                {item.label.toUpperCase()}
              </p>
              <p
                className={cn(
                  "m-0 mt-1 font-mono text-[10px] leading-5 font-bold",
                  style.value
                )}
              >
                {item.value}
              </p>
            </div>
          )
        })}
      </div>
    </div>
  )
}
