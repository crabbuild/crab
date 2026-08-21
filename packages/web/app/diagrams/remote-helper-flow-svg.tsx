"use client"

/**
 * Remote Helper Flow Diagram
 *
 * Illustrates: Git → Remote Helper → Crab Engine → Cloud Storage
 * Uses shadcn CSS variables for theme-aware colors.
 */

const PRIMARY = "var(--primary)"
const FOREGROUND = "var(--foreground)"
const MUTED_FG = "var(--muted-foreground)"
const BORDER = "var(--border)"
const MUTED = "var(--muted)"
const CARD = "var(--card)"

const ArrMarker = ({ id }: { id: string }) => (
  <defs>
    <marker
      id={id}
      markerWidth="7"
      markerHeight="5"
      refX="7"
      refY="2.5"
      orient="auto"
    >
      <path d="M0 0L7 2.5L0 5" fill="none" stroke={BORDER} strokeWidth="1" />
    </marker>
  </defs>
)

export function RemoteHelperFlowSvg() {
  return (
    <svg
      viewBox="0 0 720 200"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      className="w-full h-auto"
      role="img"
      aria-label="Remote helper flow diagram showing Git to Remote Helper to Crab Engine to Cloud Storage"
    >
      <ArrMarker id="rhf-arr" />

      {/* Git */}
      <rect
        x="10"
        y="60"
        width="120"
        height="64"
        rx="8"
        fill={MUTED}
        stroke={BORDER}
        strokeWidth="1.5"
      />
      <text
        x="70"
        y="86"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="13"
        fontWeight="600"
      >
        Git
      </text>
      <text
        x="70"
        y="106"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="10"
      >
        push / fetch / clone
      </text>

      {/* Arrow: Git → Remote Helper */}
      <line
        x1="130"
        y1="92"
        x2="185"
        y2="92"
        stroke={BORDER}
        strokeWidth="1.5"
        markerEnd="url(#rhf-arr)"
      />
      <text
        x="158"
        y="84"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        stdio
      </text>

      {/* Remote Helper */}
      <rect
        x="190"
        y="50"
        width="150"
        height="84"
        rx="8"
        fill={CARD}
        stroke={PRIMARY}
        strokeWidth="1.5"
      />
      <text
        x="265"
        y="74"
        textAnchor="middle"
        fill={PRIMARY}
        fontSize="11"
        fontWeight="700"
        letterSpacing="0.03em"
      >
        REMOTE HELPER
      </text>
      <text
        x="265"
        y="92"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="10"
      >
        git-remote-crab
      </text>
      <rect
        x="204"
        y="100"
        width="122"
        height="24"
        rx="5"
        fill={MUTED}
        stroke={BORDER}
        strokeWidth="1"
      />
      <text
        x="265"
        y="116"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        protocol translation
      </text>

      {/* Arrow: Remote Helper → Crab Engine */}
      <line
        x1="340"
        y1="92"
        x2="395"
        y2="92"
        stroke={BORDER}
        strokeWidth="1.5"
        markerEnd="url(#rhf-arr)"
      />
      <text
        x="368"
        y="84"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        API
      </text>

      {/* Crab Engine */}
      <rect
        x="400"
        y="40"
        width="160"
        height="104"
        rx="10"
        fill={CARD}
        stroke={PRIMARY}
        strokeWidth="1.5"
      />
      <text
        x="480"
        y="64"
        textAnchor="middle"
        fill={PRIMARY}
        fontSize="11"
        fontWeight="700"
        letterSpacing="0.03em"
      >
        CRAB ENGINE
      </text>
      <rect
        x="414"
        y="74"
        width="132"
        height="24"
        rx="5"
        fill={MUTED}
        stroke={BORDER}
        strokeWidth="1"
      />
      <text
        x="480"
        y="90"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        CDC · Dedup · Pack
      </text>
      <rect
        x="414"
        y="104"
        width="132"
        height="24"
        rx="5"
        fill={MUTED}
        stroke={BORDER}
        strokeWidth="1"
      />
      <text
        x="480"
        y="120"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        Metadata · Shards
      </text>

      {/* Arrow: Crab Engine → Cloud Storage */}
      <line
        x1="560"
        y1="92"
        x2="615"
        y2="92"
        stroke={BORDER}
        strokeWidth="1.5"
        markerEnd="url(#rhf-arr)"
      />
      <text
        x="588"
        y="84"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        HTTPS
      </text>

      {/* Cloud Storage */}
      <rect
        x="620"
        y="56"
        width="90"
        height="72"
        rx="8"
        fill={MUTED}
        stroke={BORDER}
        strokeWidth="1.5"
      />
      <text
        x="665"
        y="84"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="12"
        fontWeight="600"
      >
        Cloud
      </text>
      <text
        x="665"
        y="100"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="10"
      >
        Storage
      </text>
      <text
        x="665"
        y="116"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        S3 · GCS · Azure
      </text>

      {/* Bottom labels */}
      <text
        x="70"
        y="170"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        Standard git
      </text>
      <text
        x="265"
        y="170"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        Protocol bridge
      </text>
      <text
        x="480"
        y="170"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        Chunk-level processing
      </text>
      <text
        x="665"
        y="170"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        Your bucket
      </text>
    </svg>
  )
}
