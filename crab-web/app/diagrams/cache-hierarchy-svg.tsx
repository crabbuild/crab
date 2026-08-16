"use client"

/**
 * Cache Hierarchy Diagram
 *
 * Inline SVG illustrating the cache hierarchy:
 * Local LRU Cache → Cache Service → Cloud Storage
 * with latency indicators at each tier.
 *
 * Uses shadcn CSS variables for theme-aware fills/strokes.
 * Scales responsively via viewBox.
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

export function CacheHierarchySvg() {
  return (
    <svg
      viewBox="0 0 720 220"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      role="img"
      aria-label="Cache hierarchy: Local LRU Cache to Cache Service to Cloud Storage with latency indicators"
      className="w-full h-auto"
    >
      <ArrMarker id="cache-arr" />

      {/* Top title */}
      <text
        x="360"
        y="30"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="13"
        fontWeight="600"
      >
        Cache Hierarchy
      </text>

      {/* Tier 1: Local LRU Cache */}
      <rect
        x="40"
        y="60"
        width="160"
        height="70"
        rx="8"
        fill={CARD}
        stroke={PRIMARY}
        strokeWidth="1.5"
      />
      <text
        x="120"
        y="86"
        textAnchor="middle"
        fill={PRIMARY}
        fontSize="11"
        fontWeight="700"
        letterSpacing="0.03em"
      >
        Local LRU Cache
      </text>
      <text
        x="120"
        y="103"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        In-memory xorb chunks
      </text>
      <text
        x="120"
        y="117"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        per-process eviction
      </text>

      {/* Latency badge: Tier 1 */}
      <rect
        x="80"
        y="140"
        width="80"
        height="20"
        rx="10"
        fill={PRIMARY}
        opacity="0.12"
      />
      <text
        x="120"
        y="154"
        textAnchor="middle"
        fill={PRIMARY}
        fontSize="10"
        fontWeight="600"
      >
        {"<1ms"}
      </text>

      {/* Arrow 1→2 */}
      <line
        x1="200"
        y1="95"
        x2="258"
        y2="95"
        stroke={PRIMARY}
        strokeWidth="1.5"
        markerEnd="url(#cache-arr)"
      />
      <text
        x="229"
        y="88"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="8"
      >
        miss
      </text>

      {/* Tier 2: Cache Service */}
      <rect
        x="262"
        y="60"
        width="160"
        height="70"
        rx="8"
        fill={CARD}
        stroke={PRIMARY}
        strokeWidth="1.5"
      />
      <text
        x="342"
        y="86"
        textAnchor="middle"
        fill={PRIMARY}
        fontSize="11"
        fontWeight="700"
        letterSpacing="0.03em"
      >
        Cache Service
      </text>
      <text
        x="342"
        y="103"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        Shared network cache
      </text>
      <text
        x="342"
        y="117"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        metadata warming
      </text>

      {/* Latency badge: Tier 2 */}
      <rect
        x="302"
        y="140"
        width="80"
        height="20"
        rx="10"
        fill={PRIMARY}
        opacity="0.12"
      />
      <text
        x="342"
        y="154"
        textAnchor="middle"
        fill={PRIMARY}
        fontSize="10"
        fontWeight="600"
      >
        ~5ms
      </text>

      {/* Arrow 2→3 */}
      <line
        x1="422"
        y1="95"
        x2="480"
        y2="95"
        stroke={PRIMARY}
        strokeWidth="1.5"
        markerEnd="url(#cache-arr)"
      />
      <text
        x="451"
        y="88"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="8"
      >
        miss
      </text>

      {/* Tier 3: Cloud Storage */}
      <rect
        x="484"
        y="60"
        width="160"
        height="70"
        rx="8"
        fill={MUTED}
        stroke={BORDER}
        strokeWidth="1.5"
      />
      <text
        x="564"
        y="86"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="11"
        fontWeight="600"
      >
        Cloud Storage
      </text>
      <text
        x="564"
        y="103"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        S3 · GCS · Azure
      </text>
      <text
        x="564"
        y="117"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        origin of truth
      </text>

      {/* Latency badge: Tier 3 */}
      <rect
        x="524"
        y="140"
        width="80"
        height="20"
        rx="10"
        fill={BORDER}
        opacity="0.4"
      />
      <text
        x="564"
        y="154"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="10"
        fontWeight="600"
      >
        ~100ms
      </text>

      {/* Bottom flow description */}
      <text
        x="120"
        y="190"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="8"
      >
        fastest · local
      </text>
      <text
        x="342"
        y="190"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="8"
      >
        shared · warm
      </text>
      <text
        x="564"
        y="190"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="8"
      >
        durable · remote
      </text>

      {/* Direction indicator */}
      <text
        x="360"
        y="210"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
        fontStyle="italic"
      >
        lookup direction →
      </text>
    </svg>
  )
}
