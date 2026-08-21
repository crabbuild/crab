/**
 * CI Pipeline Timeline Diagram
 *
 * Compares wall-clock time for the same CI pipeline under two strategies:
 *
 *   Top row    — Default Git LFS clone: wide grey "git clone" bar (full
 *                fixture pull on every run) followed by a small "test" bar.
 *                Repeated three times across runs to show the cost compounds.
 *   Bottom row — Crab lazy checkout + runner-local chunk cache: a thin
 *                "lazy clone" bar, a small "hydrate" bar (only on the first
 *                run; subsequent runs hit the warm cache), and the same test
 *                bar. The test starts ~10× sooner.
 *
 * Server Component — uses CSS custom properties for theme adaptation.
 */

const PRIMARY = "var(--primary)"
const PRIMARY_MUTED = "var(--primary-muted)"
const FOREGROUND = "var(--foreground)"
const MUTED_FG = "var(--muted-foreground)"
const BORDER = "var(--border)"
const MUTED = "var(--muted)"

interface Segment {
  /** Width of the segment in seconds (visual). */
  duration: number
  label: string
  /** "primary" = sky highlight, "neutral" = grey, "warm" = sky-muted. */
  variant: "primary" | "neutral" | "warm"
}

interface Run {
  label: string
  segments: Segment[]
  /** Total wall-clock metric. */
  total: string
}

const lfsRuns: Run[] = [
  {
    label: "Run 1",
    segments: [
      { duration: 540, label: "git clone + lfs pull · 80 GB", variant: "neutral" },
      { duration: 60, label: "test", variant: "primary" },
    ],
    total: "10 min",
  },
  {
    label: "Run 2",
    segments: [
      { duration: 540, label: "git clone + lfs pull · 80 GB", variant: "neutral" },
      { duration: 60, label: "test", variant: "primary" },
    ],
    total: "10 min",
  },
]

const crabRuns: Run[] = [
  {
    label: "Run 1",
    segments: [
      { duration: 18, label: "lazy clone", variant: "primary" },
      { duration: 28, label: "hydrate fixtures", variant: "warm" },
      { duration: 60, label: "test", variant: "primary" },
    ],
    total: "1 min 46 s",
  },
  {
    label: "Run 2",
    segments: [
      { duration: 18, label: "lazy clone", variant: "primary" },
      { duration: 6, label: "warm cache", variant: "warm" },
      { duration: 60, label: "test", variant: "primary" },
    ],
    total: "1 min 24 s",
  },
]

const TIME_SCALE = 0.85 // pixels per second of wall-clock
const ROW_HEIGHT = 40
const ROW_GAP = 14
const LABEL_X = 16
const SEGMENT_X = 96
const METRIC_X_OFFSET = 12

function fillFor(variant: Segment["variant"]) {
  switch (variant) {
    case "primary":
      return PRIMARY
    case "warm":
      return PRIMARY_MUTED
    case "neutral":
      return MUTED
  }
}

function strokeFor(variant: Segment["variant"]) {
  switch (variant) {
    case "primary":
      return PRIMARY
    case "warm":
      return PRIMARY
    case "neutral":
      return BORDER
  }
}

function textColorFor(variant: Segment["variant"]) {
  switch (variant) {
    case "primary":
      return "var(--primary-foreground)"
    case "warm":
      return PRIMARY
    case "neutral":
      return MUTED_FG
  }
}

function RunRow({
  y,
  run,
  variant,
}: {
  y: number
  run: Run
  variant: "lfs" | "crab"
}) {
  // Pre-compute segment x-offsets so we don't mutate during render.
  const offsets: number[] = []
  let acc = 0
  for (const seg of run.segments) {
    offsets.push(acc)
    acc += seg.duration
  }
  const totalDuration = acc
  const metricX = SEGMENT_X + totalDuration * TIME_SCALE + METRIC_X_OFFSET

  return (
    <g>
      <text
        x={LABEL_X}
        y={y + 16}
        fill={FOREGROUND}
        fontSize="11"
        fontWeight="600"
      >
        {run.label}
      </text>

      {run.segments.map((segment, i) => {
        const w = segment.duration * TIME_SCALE
        const x = SEGMENT_X + offsets[i] * TIME_SCALE
        return (
          <g key={`${variant}-${run.label}-${i}`}>
            <rect
              x={x}
              y={y}
              width={Math.max(w, 1)}
              height={ROW_HEIGHT}
              rx="4"
              fill={fillFor(segment.variant)}
              stroke={strokeFor(segment.variant)}
              strokeWidth="1"
              opacity={segment.variant === "warm" ? 0.7 : 1}
            />
            {w >= 50 ? (
              <text
                x={x + w / 2}
                y={y + ROW_HEIGHT / 2 + 4}
                textAnchor="middle"
                fill={textColorFor(segment.variant)}
                fontSize="10"
                fontWeight="600"
              >
                {segment.label}
              </text>
            ) : null}
          </g>
        )
      })}

      <text
        x={metricX}
        y={y + 16}
        fill={variant === "crab" ? PRIMARY : FOREGROUND}
        fontSize="12"
        fontWeight="800"
      >
        {run.total}
      </text>
      <text x={metricX} y={y + 32} fill={MUTED_FG} fontSize="9">
        wall clock
      </text>
    </g>
  )
}

export function CiTimelineSvg() {
  const width = 760
  const height = 360

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      className="w-full h-auto"
      role="img"
      aria-label="CI pipeline timeline. Git LFS spends about 9 minutes pulling 80 gigabytes of fixtures on every run before a 1-minute test step. Crab uses a lazy clone plus a runner-local chunk cache, so the same test runs after about 18 seconds on a warm runner."
    >
      {/* Title */}
      <text
        x={width / 2}
        y="28"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="14"
        fontWeight="600"
      >
        Same CI job. Same test. 10× faster feedback.
      </text>

      {/* Legend */}
      <g transform="translate(16 50)">
        <rect x="0" y="0" width="14" height="12" rx="2" fill={PRIMARY} />
        <text x="20" y="10" fill={MUTED_FG} fontSize="10">
          fast — local
        </text>
        <rect
          x="120"
          y="0"
          width="14"
          height="12"
          rx="2"
          fill={PRIMARY_MUTED}
          stroke={PRIMARY}
          strokeWidth="1"
          opacity="0.7"
        />
        <text x="140" y="10" fill={MUTED_FG} fontSize="10">
          warm cache
        </text>
        <rect
          x="240"
          y="0"
          width="14"
          height="12"
          rx="2"
          fill={MUTED}
          stroke={BORDER}
          strokeWidth="1"
        />
        <text x="260" y="10" fill={MUTED_FG} fontSize="10">
          full network pull
        </text>
      </g>

      {/* Git LFS section */}
      <text
        x={LABEL_X}
        y={88}
        fill={FOREGROUND}
        fontSize="12"
        fontWeight="700"
      >
        Git LFS
      </text>
      <text x={LABEL_X} y={104} fill={MUTED_FG} fontSize="10">
        Each run re-clones the full fixture suite.
      </text>
      <RunRow y={114} run={lfsRuns[0]} variant="lfs" />
      <RunRow y={114 + ROW_HEIGHT + ROW_GAP} run={lfsRuns[1]} variant="lfs" />

      {/* Crab section */}
      <text
        x={LABEL_X}
        y={234}
        fill={FOREGROUND}
        fontSize="12"
        fontWeight="700"
      >
        Crab
      </text>
      <text x={LABEL_X} y={250} fill={MUTED_FG} fontSize="10">
        Lazy checkout + runner-local chunk cache.
      </text>
      <RunRow y={260} run={crabRuns[0]} variant="crab" />
      <RunRow y={260 + ROW_HEIGHT + ROW_GAP} run={crabRuns[1]} variant="crab" />

      {/* Footer caption */}
      <text
        x={width / 2}
        y={height - 6}
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="10"
      >
        Bars are to scale (1 px ≈ 1.2 s). Test step is identical in both rows.
      </text>
    </svg>
  )
}
