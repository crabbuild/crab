"use client"

/**
 * CLI Push Pipeline Diagram
 *
 * Inline SVG illustrating the push data flow:
 * Local Files → CDC (Content-Defined Chunking) → Staging → Upload → Cloud Storage
 *
 * Uses shadcn CSS variables for theme-aware fills/strokes.
 * Scales responsively via viewBox — fills the container width.
 */

const PRIMARY = "var(--primary)"
const BORDER = "var(--border)"
const MUTED = "var(--muted)"
const FOREGROUND = "var(--foreground)"
const MUTED_FG = "var(--muted-foreground)"
const CARD = "var(--card)"

function ArrMarker({ id }: { id: string }) {
  return (
    <defs>
      <marker
        id={id}
        markerWidth="7"
        markerHeight="5"
        refX="7"
        refY="2.5"
        orient="auto"
      >
        <path
          d="M0 0L7 2.5L0 5"
          fill="none"
          stroke={PRIMARY}
          strokeWidth="1.5"
        />
      </marker>
    </defs>
  )
}

export function CliPushPipelineSvg() {
  // Layout constants for a wider, taller diagram
  const boxW = 160
  const boxH = 80
  const gap = 40
  const arrowLen = gap - 4
  const startX = 20
  const boxY = 60
  const totalW = startX * 2 + boxW * 5 + gap * 4 // 940
  const totalH = 200

  const stages = [
    {
      title: "Local Files",
      subtitle: "tracked by crab",
      bottomLabel: "crab add",
      highlight: false,
    },
    {
      title: "CDC",
      subtitle: "Content-Defined Chunking",
      bottomLabel: "gearhash split",
      highlight: true,
    },
    {
      title: "Staging",
      subtitle: "Dedup + Index",
      bottomLabel: "3-tier dedup",
      highlight: true,
    },
    {
      title: "Upload",
      subtitle: "Pack Xorbs (~64 MiB)",
      bottomLabel: "parallel transfer",
      highlight: true,
    },
    {
      title: "Cloud",
      subtitle: "S3 · GCS · Azure",
      bottomLabel: "object store",
      highlight: false,
    },
  ]

  return (
    <svg
      viewBox={`0 0 ${totalW} ${totalH}`}
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      role="img"
      aria-label="CLI push pipeline: Local Files to CDC to Staging to Upload to Cloud Storage"
      className="w-full h-auto"
    >
      <ArrMarker id="pipeline-arr" />

      {/* Title */}
      <text
        x={totalW / 2}
        y="32"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="16"
        fontWeight="600"
      >
        Push Pipeline
      </text>

      {stages.map((stage, i) => {
        const x = startX + i * (boxW + gap)
        const centerX = x + boxW / 2
        const centerY = boxY + boxH / 2

        return (
          <g key={stage.title}>
            {/* Box */}
            <rect
              x={x}
              y={boxY}
              width={boxW}
              height={boxH}
              rx="10"
              fill={stage.highlight ? CARD : MUTED}
              stroke={stage.highlight ? PRIMARY : BORDER}
              strokeWidth="1.5"
            />

            {/* Title */}
            <text
              x={centerX}
              y={centerY - 8}
              textAnchor="middle"
              fill={stage.highlight ? PRIMARY : FOREGROUND}
              fontSize="14"
              fontWeight="700"
              letterSpacing="0.02em"
            >
              {stage.title}
            </text>

            {/* Subtitle */}
            <text
              x={centerX}
              y={centerY + 12}
              textAnchor="middle"
              fill={MUTED_FG}
              fontSize="11"
            >
              {stage.subtitle}
            </text>

            {/* Bottom label */}
            <text
              x={centerX}
              y={boxY + boxH + 24}
              textAnchor="middle"
              fill={MUTED_FG}
              fontSize="10"
            >
              {stage.bottomLabel}
            </text>

            {/* Arrow to next stage */}
            {i < stages.length - 1 && (
              <line
                x1={x + boxW}
                y1={boxY + boxH / 2}
                x2={x + boxW + arrowLen}
                y2={boxY + boxH / 2}
                stroke={PRIMARY}
                strokeWidth="2"
                markerEnd="url(#pipeline-arr)"
              />
            )}
          </g>
        )
      })}
    </svg>
  )
}
