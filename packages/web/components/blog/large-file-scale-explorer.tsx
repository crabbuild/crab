"use client"

import { Check, Cloud, GitBranch, HardDrive, Waves } from "lucide-react"
import { useId, useMemo, useState } from "react"

import { cn } from "@/lib/utils"

const REPOSITORY_SIZES = [
  { label: "50 GB", gigabytes: 50, context: "A design or media repository" },
  {
    label: "500 GB",
    gigabytes: 500,
    context: "A growing model and dataset history",
  },
  {
    label: "5 TB",
    gigabytes: 5_000,
    context: "A multi-team production repository",
  },
] as const

const CHUNK_COUNT = 36

function formatDataSize(gigabytes: number) {
  if (gigabytes >= 1_000) {
    return `${(gigabytes / 1_000).toLocaleString(undefined, {
      maximumFractionDigits: 2,
    })} TB`
  }

  if (gigabytes >= 1) {
    return `${gigabytes.toLocaleString(undefined, {
      maximumFractionDigits: 1,
    })} GB`
  }

  return `${Math.round(gigabytes * 1_000).toLocaleString()} MB`
}

function ScaleIcon({ type }: { type: "history" | "new" | "reuse" }) {
  const Icon =
    type === "history" ? GitBranch : type === "new" ? Cloud : HardDrive

  return <Icon size={15} aria-hidden="true" />
}

export function LargeFileScaleExplorer() {
  const rangeId = useId()
  const [sizeIndex, setSizeIndex] = useState(1)
  const [changedPercent, setChangedPercent] = useState(5)
  const repository = REPOSITORY_SIZES[sizeIndex]
  const newData = repository.gigabytes * (changedPercent / 100)
  const reusableData = repository.gigabytes - newData
  const changedChunks = Math.max(
    1,
    Math.round(CHUNK_COUNT * (changedPercent / 100))
  )
  const changeStart = Math.floor((CHUNK_COUNT - changedChunks) * 0.58)

  const chunks = useMemo(
    () =>
      Array.from({ length: CHUNK_COUNT }, (_, index) => ({
        index,
        changed: index >= changeStart && index < changeStart + changedChunks,
      })),
    [changeStart, changedChunks]
  )

  return (
    <figure className="not-prose relative mx-auto mt-12 w-full max-w-6xl overflow-hidden rounded-[1.75rem] border border-white/10 bg-[#07111d] text-slate-100 shadow-[0_40px_120px_rgba(1,8,18,0.55)]">
      <div
        className="pointer-events-none absolute inset-0 opacity-70"
        aria-hidden="true"
        style={{
          background:
            "radial-gradient(circle at 17% 0%, rgba(34,211,238,.14), transparent 34%), radial-gradient(circle at 88% 68%, rgba(125,211,252,.12), transparent 32%)",
        }}
      />

      <div className="relative grid gap-0 xl:grid-cols-[19rem_minmax(0,1fr)]">
        <div className="border-b border-white/10 bg-white/[0.025] p-5 sm:p-7 xl:border-r xl:border-b-0">
          <div className="flex items-center gap-2 font-mono text-[10px] font-semibold tracking-[0.2em] text-cyan-300 uppercase">
            <Waves size={14} aria-hidden="true" />
            Scale lens
          </div>
          <h2 className="mt-4 text-xl leading-tight font-semibold tracking-tight text-white sm:text-2xl">
            Change the scale. Keep the same Git workflow.
          </h2>
          <p className="mt-3 text-sm leading-6 text-slate-400">
            Model one new version of a large repository. The illustration
            separates its logical size from the chunk data that changed.
          </p>

          <fieldset className="mt-7">
            <legend className="font-mono text-[10px] font-semibold tracking-[0.16em] text-slate-500 uppercase">
              Repository version
            </legend>
            <div className="mt-3 grid grid-cols-3 gap-2 xl:grid-cols-1">
              {REPOSITORY_SIZES.map((size, index) => {
                const selected = index === sizeIndex

                return (
                  <button
                    key={size.label}
                    type="button"
                    aria-pressed={selected}
                    onClick={() => setSizeIndex(index)}
                    className={cn(
                      "group min-h-11 rounded-lg border px-3 py-2 text-left transition-colors focus-visible:ring-2 focus-visible:ring-cyan-300 focus-visible:ring-offset-2 focus-visible:ring-offset-[#07111d] focus-visible:outline-none",
                      selected
                        ? "border-cyan-300/50 bg-cyan-300/10 text-cyan-100"
                        : "border-white/10 bg-white/[0.025] text-slate-400 hover:border-white/20 hover:text-slate-200"
                    )}
                  >
                    <span className="flex items-center justify-between gap-2 text-sm font-semibold">
                      {size.label}
                      {selected && <Check size={14} aria-hidden="true" />}
                    </span>
                    <span className="mt-0.5 hidden text-[11px] leading-4 text-slate-500 xl:block">
                      {size.context}
                    </span>
                  </button>
                )
              })}
            </div>
          </fieldset>

          <div className="mt-7">
            <div className="flex items-end justify-between gap-4">
              <label
                htmlFor={rangeId}
                className="font-mono text-[10px] font-semibold tracking-[0.16em] text-slate-500 uppercase"
              >
                Content changed
              </label>
              <output
                htmlFor={rangeId}
                className="font-mono text-lg font-semibold text-amber-300"
              >
                {changedPercent}%
              </output>
            </div>
            <input
              id={rangeId}
              type="range"
              min="1"
              max="35"
              step="1"
              value={changedPercent}
              onChange={(event) =>
                setChangedPercent(Number(event.target.value))
              }
              className="mt-3 h-11 w-full cursor-pointer accent-amber-300"
            />
            <div
              className="flex justify-between font-mono text-[9px] text-slate-600"
              aria-hidden="true"
            >
              <span>LOCAL EDIT</span>
              <span>HEAVY CHANGE</span>
            </div>
          </div>
        </div>

        <div className="relative min-w-0 p-4 sm:p-7">
          <div className="grid gap-3 sm:grid-cols-3" aria-live="polite">
            <ScaleMetric
              label="Logical version"
              value={repository.label}
              detail="Named by one Git commit"
              type="history"
            />
            <ScaleMetric
              label="New chunk data"
              value={formatDataSize(newData)}
              detail="Illustrative changed regions"
              type="new"
              tone="changed"
            />
            <ScaleMetric
              label="Reuse candidate"
              value={formatDataSize(reusableData)}
              detail="Matched by chunk identity"
              type="reuse"
              tone="reused"
            />
          </div>

          <div className="mt-5 [scrollbar-width:thin] [scrollbar-color:#334155_#040a12] overflow-x-auto rounded-xl border border-white/10 bg-[#040a12]">
            <svg
              viewBox="0 0 820 360"
              className="block h-auto min-h-[20rem] w-full min-w-[43rem]"
              role="img"
              aria-labelledby={`${rangeId}-title ${rangeId}-description`}
            >
              <title id={`${rangeId}-title`}>
                {`Git history and Crab data paths for a ${repository.label} version`}
              </title>
              <desc id={`${rangeId}-description`}>
                {`Git stores a compact pointer in history while Crab maps changed chunks to cloud object storage and reuses unchanged chunk identities. The current illustration marks ${changedPercent} percent of the logical version as changed.`}
              </desc>
              <defs>
                <linearGradient
                  id={`${rangeId}-git`}
                  x1="0"
                  y1="0"
                  x2="1"
                  y2="0"
                >
                  <stop offset="0" stopColor="#fb923c" stopOpacity="0.18" />
                  <stop offset="1" stopColor="#fb923c" stopOpacity="0.03" />
                </linearGradient>
                <linearGradient
                  id={`${rangeId}-data`}
                  x1="0"
                  y1="0"
                  x2="1"
                  y2="0"
                >
                  <stop offset="0" stopColor="#22d3ee" stopOpacity="0.16" />
                  <stop offset="1" stopColor="#38bdf8" stopOpacity="0.03" />
                </linearGradient>
                <marker
                  id={`${rangeId}-arrow`}
                  markerWidth="7"
                  markerHeight="7"
                  refX="6"
                  refY="3.5"
                  orient="auto"
                >
                  <path d="M0 0 7 3.5 0 7Z" fill="#64748b" />
                </marker>
              </defs>

              <g fontFamily="ui-monospace, SFMono-Regular, Menlo, monospace">
                <text
                  x="24"
                  y="27"
                  fill="#64748b"
                  fontSize="10"
                  letterSpacing="1.8"
                >
                  ONE COMMIT · TWO PHYSICAL LANES
                </text>

                <rect
                  x="24"
                  y="55"
                  width="128"
                  height="72"
                  rx="12"
                  fill="#0d1724"
                  stroke="#475569"
                />
                <text
                  x="88"
                  y="82"
                  textAnchor="middle"
                  fill="#e2e8f0"
                  fontSize="12"
                  fontWeight="650"
                >
                  Worktree
                </text>
                <text
                  x="88"
                  y="103"
                  textAnchor="middle"
                  fill="#64748b"
                  fontSize="10"
                >
                  {repository.label} of files
                </text>

                <path
                  d="M152 91 H214"
                  fill="none"
                  stroke="#64748b"
                  markerEnd={`url(#${rangeId}-arrow)`}
                />
                <rect
                  x="216"
                  y="55"
                  width="132"
                  height="72"
                  rx="12"
                  fill="#0d1724"
                  stroke="#a78bfa"
                />
                <text
                  x="282"
                  y="82"
                  textAnchor="middle"
                  fill="#e2e8f0"
                  fontSize="12"
                  fontWeight="650"
                >
                  Crab filter
                </text>
                <text
                  x="282"
                  y="103"
                  textAnchor="middle"
                  fill="#a78bfa"
                  fontSize="10"
                >
                  hash · chunk · map
                </text>

                <path
                  d="M348 78 C390 78 386 62 430 62"
                  fill="none"
                  stroke="#fb923c"
                  strokeOpacity="0.8"
                  markerEnd={`url(#${rangeId}-arrow)`}
                />
                <path
                  d="M348 105 C390 105 386 228 430 228"
                  fill="none"
                  stroke="#22d3ee"
                  strokeOpacity="0.8"
                  markerEnd={`url(#${rangeId}-arrow)`}
                />

                <rect
                  x="432"
                  y="40"
                  width="356"
                  height="88"
                  rx="14"
                  fill={`url(#${rangeId}-git)`}
                  stroke="#fb923c"
                  strokeOpacity="0.45"
                />
                <text
                  x="454"
                  y="65"
                  fill="#fdba74"
                  fontSize="10"
                  letterSpacing="1.4"
                >
                  GIT HISTORY
                </text>
                <circle cx="477" cy="96" r="8" fill="#fb923c" />
                <circle cx="545" cy="96" r="8" fill="#fb923c" />
                <circle cx="613" cy="96" r="8" fill="#fb923c" />
                <path
                  d="M485 96 H537 M553 96 H605"
                  stroke="#fb923c"
                  strokeWidth="2"
                />
                <rect
                  x="655"
                  y="76"
                  width="110"
                  height="39"
                  rx="8"
                  fill="#111827"
                  stroke="#fb923c"
                  strokeOpacity="0.75"
                />
                <text
                  x="710"
                  y="92"
                  textAnchor="middle"
                  fill="#fed7aa"
                  fontSize="10"
                >
                  pointer blob
                </text>
                <text
                  x="710"
                  y="107"
                  textAnchor="middle"
                  fill="#64748b"
                  fontSize="9"
                >
                  hash + size
                </text>

                <rect
                  x="432"
                  y="164"
                  width="356"
                  height="156"
                  rx="14"
                  fill={`url(#${rangeId}-data)`}
                  stroke="#22d3ee"
                  strokeOpacity="0.38"
                />
                <text
                  x="454"
                  y="190"
                  fill="#67e8f9"
                  fontSize="10"
                  letterSpacing="1.4"
                >
                  CRAB DATA PLANE
                </text>
                <text
                  x="765"
                  y="190"
                  textAnchor="end"
                  fill="#64748b"
                  fontSize="9"
                >
                  CONTENT-DEFINED REGIONS
                </text>

                {chunks.map((chunk) => {
                  const column = chunk.index % 18
                  const row = Math.floor(chunk.index / 18)
                  const x = 454 + column * 17.2
                  const y = 210 + row * 33

                  return (
                    <rect
                      key={chunk.index}
                      x={x}
                      y={y}
                      width="12"
                      height="24"
                      rx="3"
                      fill={chunk.changed ? "#fbbf24" : "#22d3ee"}
                      fillOpacity={chunk.changed ? 0.9 : 0.45}
                      stroke={chunk.changed ? "#fde68a" : "#67e8f9"}
                      strokeOpacity={chunk.changed ? 0.8 : 0.28}
                    />
                  )
                })}

                <g transform="translate(454 291)">
                  <rect
                    width="10"
                    height="10"
                    rx="2"
                    fill="#22d3ee"
                    fillOpacity="0.5"
                  />
                  <text x="17" y="9" fill="#94a3b8" fontSize="9">
                    unchanged identity
                  </text>
                  <rect x="138" width="10" height="10" rx="2" fill="#fbbf24" />
                  <text x="155" y="9" fill="#94a3b8" fontSize="9">
                    new chunk
                  </text>
                  <path
                    d="M246 5 H278"
                    stroke="#64748b"
                    markerEnd={`url(#${rangeId}-arrow)`}
                  />
                  <text x="288" y="9" fill="#7dd3fc" fontSize="9">
                    your object store
                  </text>
                </g>
              </g>
            </svg>
          </div>

          <figcaption className="mt-4 flex gap-3 text-xs leading-5 text-slate-500">
            <span
              className="mt-1 block h-px w-8 shrink-0 bg-slate-700"
              aria-hidden="true"
            />
            This is an explanatory model, not a benchmark or savings promise.
            Actual reuse follows content-defined chunk identities in your data.
          </figcaption>
        </div>
      </div>
    </figure>
  )
}

function ScaleMetric({
  label,
  value,
  detail,
  type,
  tone = "default",
}: {
  label: string
  value: string
  detail: string
  type: "history" | "new" | "reuse"
  tone?: "default" | "changed" | "reused"
}) {
  return (
    <div className="rounded-xl border border-white/10 bg-white/[0.035] p-4">
      <div
        className={cn(
          "flex items-center gap-2 text-[11px] font-medium",
          tone === "changed"
            ? "text-amber-300"
            : tone === "reused"
              ? "text-cyan-300"
              : "text-slate-400"
        )}
      >
        <ScaleIcon type={type} />
        {label}
      </div>
      <div className="mt-2 text-2xl font-semibold tracking-tight text-white">
        {value}
      </div>
      <div className="mt-1 text-[11px] text-slate-500">{detail}</div>
    </div>
  )
}
