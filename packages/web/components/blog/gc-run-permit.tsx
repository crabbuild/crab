"use client"

import {
  ArchiveRestore,
  Check,
  FileClock,
  LockKeyhole,
  PackageOpen,
  Timer,
  X,
} from "lucide-react"
import { useState } from "react"

import { cn } from "@/lib/utils"

type PermitCase = {
  id: "preview" | "apply" | "bucket" | "force"
  label: string
  title: string
  command: string
  status: "PREVIEW" | "READY" | "REFUSED" | "CAUTION"
  checks: { label: string; ok: boolean; note: string }[]
  action: string
}

const PERMIT_CASES: PermitCase[] = [
  {
    id: "preview",
    label: "Repo preview",
    title: "Review the proof without deleting",
    command: "crab gc --scope repo --dry-run --json",
    status: "PREVIEW",
    checks: [
      { label: "Repository scope", ok: true, note: "normal boundary" },
      { label: "Root walk complete", ok: true, note: "no unknown proof" },
      { label: "Dry run", ok: true, note: "zero deletes" },
      { label: "Candidate inventory", ok: true, note: "save for review" },
    ],
    action:
      "Record candidate counts, bytes, policy, and the exact repository identity.",
  },
  {
    id: "apply",
    label: "Repo apply",
    title: "Apply the reviewed repository plan",
    command: "crab gc --scope repo",
    status: "READY",
    checks: [
      { label: "Dry run reviewed", ok: true, note: "same scope + policy" },
      { label: "Roots readable", ok: true, note: "mark proof complete" },
      { label: "Sweep fence", ok: true, note: "writer epoch sealed" },
      { label: "Post-check planned", ok: true, note: "run crab fsck" },
    ],
    action: "Apply, preserve the outcome, then run integrity verification.",
  },
  {
    id: "bucket",
    label: "Bucket incomplete",
    title: "Bucket scope cannot prove shared reachability",
    command: "crab gc --scope bucket --bucket team-data",
    status: "REFUSED",
    checks: [
      { label: "Bucket named", ok: true, note: "team-data" },
      { label: "Registry complete", ok: false, note: "repo/labs missing" },
      { label: "Shared marks complete", ok: false, note: "blind spot" },
      { label: "Coordinator proof", ok: true, note: "still insufficient" },
    ],
    action: "Repair registry coverage before attempting destructive bucket GC.",
  },
  {
    id: "force",
    label: "Force recent",
    title: "Force removes only the age key",
    command: "crab gc --scope repo --force",
    status: "CAUTION",
    checks: [
      { label: "Age protection", ok: false, note: "bypassed" },
      { label: "Reachability proof", ok: true, note: "still required" },
      { label: "Writer fence", ok: true, note: "still required" },
      { label: "Confirmation", ok: true, note: "interactive or --yes" },
    ],
    action:
      "Use only with a documented maintenance reason and current coordination evidence.",
  },
]

const PERMIT_STYLE = {
  PREVIEW: "bg-[#dcecf4] text-[#285b79]",
  READY: "bg-[#dcece6] text-[#205743]",
  REFUSED: "bg-[#f7d9d5] text-[#7a2923]",
  CAUTION: "bg-[#fff0c9] text-[#714d12]",
}

export function GcRunPermit() {
  const [caseId, setCaseId] = useState<PermitCase["id"]>("preview")
  const selected =
    PERMIT_CASES.find((item) => item.id === caseId) ?? PERMIT_CASES[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.75rem] border border-[#aebdc1] bg-[#eef4f3] text-[#20313a] shadow-[0_20px_60px_rgba(19,42,58,0.14)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="grid gap-5 border-b border-[#aebdc1] px-5 py-5 sm:px-7 lg:grid-cols-[1fr_auto] lg:items-end">
        <div>
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-[#2e6f95]">
            DELETION PERMIT / TRY AN OPERATION
          </p>
          <h3 className="m-0 mt-2 text-2xl font-black tracking-[-0.04em] sm:text-3xl">
            Does this run have enough proof?
          </h3>
        </div>
        <FileClock
          className="hidden size-8 text-[#c64e44] lg:block"
          aria-hidden="true"
        />
      </header>

      <div className="grid lg:grid-cols-[17rem_1fr]">
        <nav
          className="border-b border-[#aebdc1] p-5 sm:p-7 lg:border-r lg:border-b-0"
          aria-label="GC operation"
        >
          <div className="grid gap-2 sm:grid-cols-2 lg:grid-cols-1">
            {PERMIT_CASES.map((item) => (
              <button
                key={item.id}
                type="button"
                aria-pressed={selected.id === item.id}
                onClick={() => setCaseId(item.id)}
                className={cn(
                  "min-h-11 rounded-xl border px-4 py-3 text-left text-sm font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#2e6f95] focus-visible:ring-offset-2",
                  selected.id === item.id
                    ? "border-[#132a3a] bg-[#132a3a] text-white"
                    : "border-[#aebdc1] bg-white text-[#536871] hover:border-[#132a3a] hover:text-[#132a3a]"
                )}
              >
                {item.label}
              </button>
            ))}
          </div>
        </nav>

        <section className="bg-white p-5 sm:p-7" aria-live="polite">
          <div className="flex flex-wrap items-start justify-between gap-4">
            <div>
              <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#667a83]">
                OPERATOR DECISION
              </p>
              <h4 className="m-0 mt-1 text-xl font-black sm:text-2xl">
                {selected.title}
              </h4>
            </div>
            <span
              className={cn(
                "rounded-full px-3 py-2 font-mono text-[10px] font-black",
                PERMIT_STYLE[selected.status]
              )}
            >
              {selected.status}
            </span>
          </div>

          <code className="mt-5 block overflow-x-auto rounded-xl bg-[#132a3a] px-4 py-3 font-mono text-[11px] text-[#f6c85f]">
            {selected.command}
          </code>

          <div className="mt-5 grid gap-3 sm:grid-cols-2">
            {selected.checks.map((check) => (
              <div
                key={check.label}
                className="rounded-xl border border-[#aebdc1] bg-[#eef4f3] p-4"
              >
                <div className="flex items-center justify-between gap-3">
                  <p className="m-0 text-sm font-black">{check.label}</p>
                  {check.ok ? (
                    <Check
                      className="size-5 text-[#2f7d63]"
                      aria-hidden="true"
                    />
                  ) : (
                    <X className="size-5 text-[#c64e44]" aria-hidden="true" />
                  )}
                </div>
                <p className="m-0 mt-2 font-mono text-[9px] text-[#667a83]">
                  {check.note}
                </p>
              </div>
            ))}
          </div>

          <div className="mt-5 flex gap-3 rounded-xl border border-dashed border-[#8fa4aa] bg-[#f7f9f8] p-4">
            {selected.status === "READY" ? (
              <PackageOpen
                className="mt-0.5 size-5 shrink-0 text-[#2f7d63]"
                aria-hidden="true"
              />
            ) : selected.status === "REFUSED" ? (
              <LockKeyhole
                className="mt-0.5 size-5 shrink-0 text-[#c64e44]"
                aria-hidden="true"
              />
            ) : selected.status === "CAUTION" ? (
              <Timer
                className="mt-0.5 size-5 shrink-0 text-[#d99a27]"
                aria-hidden="true"
              />
            ) : (
              <ArchiveRestore
                className="mt-0.5 size-5 shrink-0 text-[#2e6f95]"
                aria-hidden="true"
              />
            )}
            <p className="m-0 text-sm leading-6 text-[#536871]">
              {selected.action}
            </p>
          </div>
        </section>
      </div>
      <figcaption className="border-t border-[#aebdc1] px-5 py-3 font-mono text-[10px] text-[#667a83] sm:px-7">
        Force bypasses age protection only. Unknown reachability always blocks
        deletion.
      </figcaption>
    </figure>
  )
}
