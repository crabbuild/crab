/**
 * Hero Architecture Diagram
 *
 * High-level overview shown in the landing page hero:
 *   Git Repo  →  Crab  →  Cloud Storage
 *
 * Implemented as an HTML/React component with Tailwind CSS for proper
 * responsive layout. Uses CSS custom properties for dark/light mode.
 */

import { GitBranch, Cloud } from "lucide-react"

function Arrow({ label }: { label: string }) {
  return (
    <div className="flex flex-col items-center justify-center gap-1.5 px-1 py-4 md:py-0">
      <span className="whitespace-nowrap text-[11px] text-muted-foreground">
        {label}
      </span>
      <svg
        width="56"
        height="14"
        viewBox="0 0 56 14"
        fill="none"
        className="text-primary"
      >
        <line
          x1="0"
          y1="7"
          x2="48"
          y2="7"
          stroke="currentColor"
          strokeWidth="1.8"
        />
        <path
          d="M44 3L54 7L44 11"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.8"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
      </svg>
    </div>
  )
}

export function HeroArchitectureSvg() {
  return (
    <div
      className="mx-auto w-full max-w-[860px]"
      role="img"
      aria-label="Crab architecture overview: Git repository on the left, Crab in the middle, Cloud storage on the right, connected by flow arrows"
    >
      {/* Title */}
      <p className="mb-6 text-center text-sm font-semibold text-foreground md:mb-8 md:text-base">
        Serverless Git for Large Files
      </p>

      {/* 3-column diagram */}
      <div className="grid grid-cols-1 items-center gap-0 md:grid-cols-[1fr_auto_1.3fr_auto_1fr]">
        {/* ── Git Repo ── */}
        <div className="rounded-xl border border-border bg-muted/60 p-5 md:p-6">
          <h4 className="mb-4 text-center text-[13px] font-bold tracking-wide text-foreground">
            Git Repo
          </h4>
          {/* Git branch icon */}
          <div className="mb-3 flex justify-center">
            <GitBranch size={40} strokeWidth={1.6} className="text-primary" />
          </div>
          <div className="flex flex-col items-center gap-1.5">
            <span className="text-[11px] text-muted-foreground">commits</span>
            <span className="text-[11px] text-muted-foreground">branches</span>
            <span className="text-[11px] text-muted-foreground">
              pointer blobs
            </span>
          </div>
        </div>

        {/* ── Arrow 1 ── */}
        <Arrow label="push / pull" />

        {/* ── Crab Engine ── */}
        <div className="rounded-xl border-2 border-primary bg-primary-muted p-5 md:p-6">
          <h4 className="mb-0.5 text-center text-base font-extrabold tracking-wide text-primary">
            Crab
          </h4>
          <p className="mb-4 text-center text-[11px] text-muted-foreground">
            git remote helper
          </p>
          <div className="flex flex-col gap-2">
            <div className="rounded-lg border border-border bg-card px-4 py-2.5 text-center text-[12px] font-medium text-foreground">
              CDC chunking
            </div>
            <div className="rounded-lg border border-border bg-card px-4 py-2.5 text-center text-[12px] font-medium text-foreground">
              Dedup &amp; pack
            </div>
            <div className="rounded-lg border border-border bg-card px-4 py-2.5 text-center text-[12px] font-medium text-foreground">
              Filter &amp; VFS
            </div>
          </div>
        </div>

        {/* ── Arrow 2 ── */}
        <Arrow label="xorbs / shards" />

        {/* ── Cloud Storage ── */}
        <div className="rounded-xl border border-border bg-muted/60 p-5 md:p-6">
          <h4 className="mb-4 text-center text-[13px] font-bold tracking-wide text-foreground">
            Cloud Storage
          </h4>
          {/* Cloud icon */}
          <div className="mb-3 flex justify-center">
            <Cloud size={40} strokeWidth={1.6} className="text-primary" />
          </div>
          <div className="flex flex-col items-center gap-1.5">
            <span className="text-[11px] text-muted-foreground">AWS S3</span>
            <span className="text-[11px] text-muted-foreground">
              Google Cloud Storage
            </span>
            <span className="text-[11px] text-muted-foreground">
              Azure Blob
            </span>
          </div>
        </div>
      </div>

      {/* Bottom captions */}
      <div className="mt-5 hidden grid-cols-[1fr_auto_1.3fr_auto_1fr] gap-0 md:grid">
        <p className="text-center text-[11px] text-muted-foreground">
          Unmodified Git workflow
        </p>
        <div />
        <p className="text-center text-[11px] text-muted-foreground">
          Single binary, no servers
        </p>
        <div />
        <p className="text-center text-[11px] text-muted-foreground">
          Your bucket, your data
        </p>
      </div>
    </div>
  )
}
