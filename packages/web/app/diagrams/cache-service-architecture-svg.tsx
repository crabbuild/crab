"use client"

/**
 * Cache Service Architecture Diagram
 *
 * Shows the request flow: Crab CLI → Cache Service → Origin (S3/GCS/Azure)
 * with internal components: Auth, Handlers, Cache Store (SQLite + NVMe),
 * Chunk Index, Evictor, and Metrics.
 *
 * Uses CSS custom properties for theme-aware fills/strokes.
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

export function CacheServiceArchitectureSvg() {
  return (
    <svg
      viewBox="0 0 800 380"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      role="img"
      aria-label="Cache service architecture: Crab clients connect through auth middleware to the cache service, which stores objects on NVMe disk with SQLite metadata and falls back to cloud origin on cache miss"
      className="w-full h-auto"
    >
      <ArrMarker id="arch-arr" />

      {/* Left: Crab Clients */}
      <rect
        x="20"
        y="130"
        width="120"
        height="120"
        rx="8"
        fill={CARD}
        stroke={BORDER}
        strokeWidth="1.5"
      />
      <text
        x="80"
        y="160"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="12"
        fontWeight="600"
      >
        Crab Clients
      </text>
      <text x="80" y="180" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        clone · fetch
      </text>
      <text x="80" y="195" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        hydrate · push
      </text>
      <text x="80" y="215" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        CI runners
      </text>
      <text x="80" y="235" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        dev workstations
      </text>

      {/* Arrow: Clients → Cache Service */}
      <line
        x1="140"
        y1="190"
        x2="195"
        y2="190"
        stroke={PRIMARY}
        strokeWidth="1.5"
        markerEnd="url(#arch-arr)"
      />
      <text x="167" y="183" textAnchor="middle" fill={MUTED_FG} fontSize="8">
        HTTPS
      </text>

      {/* Center: Cache Service box */}
      <rect
        x="200"
        y="30"
        width="380"
        height="320"
        rx="10"
        fill={CARD}
        stroke={PRIMARY}
        strokeWidth="1.5"
        strokeDasharray="4 2"
      />
      <text
        x="390"
        y="55"
        textAnchor="middle"
        fill={PRIMARY}
        fontSize="13"
        fontWeight="700"
        letterSpacing="0.04em"
      >
        crab-cache-server
      </text>

      {/* Auth Middleware */}
      <rect
        x="220"
        y="75"
        width="140"
        height="40"
        rx="6"
        fill={MUTED}
        stroke={BORDER}
        strokeWidth="1"
      />
      <text
        x="290"
        y="99"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="10"
        fontWeight="500"
      >
        Auth (PSK / Bearer / mTLS)
      </text>

      {/* Handlers */}
      <rect
        x="220"
        y="130"
        width="140"
        height="90"
        rx="6"
        fill={MUTED}
        stroke={BORDER}
        strokeWidth="1"
      />
      <text
        x="290"
        y="150"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="10"
        fontWeight="500"
      >
        Handlers
      </text>
      <text x="290" y="168" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        GET / PUT objects
      </text>
      <text x="290" y="183" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        POST dedup query
      </text>
      <text x="290" y="198" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        Range requests
      </text>
      <text x="290" y="213" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        Health + Metrics
      </text>

      {/* Cache Store */}
      <rect
        x="220"
        y="235"
        width="140"
        height="60"
        rx="6"
        fill={MUTED}
        stroke={PRIMARY}
        strokeWidth="1"
        opacity="0.9"
      />
      <text
        x="290"
        y="258"
        textAnchor="middle"
        fill={PRIMARY}
        fontSize="10"
        fontWeight="600"
      >
        Cache Store
      </text>
      <text x="290" y="275" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        NVMe disk + SQLite metadata
      </text>
      <text x="290" y="290" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        blake3-verified writes
      </text>

      {/* Chunk Index */}
      <rect
        x="400"
        y="130"
        width="140"
        height="55"
        rx="6"
        fill={MUTED}
        stroke={BORDER}
        strokeWidth="1"
      />
      <text
        x="470"
        y="153"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="10"
        fontWeight="500"
      >
        Chunk Index
      </text>
      <text x="470" y="170" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        Cross-repo dedup
      </text>
      <text x="470" y="183" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        100k chunks/query
      </text>

      {/* Evictor */}
      <rect
        x="400"
        y="200"
        width="140"
        height="50"
        rx="6"
        fill={MUTED}
        stroke={BORDER}
        strokeWidth="1"
      />
      <text
        x="470"
        y="222"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="10"
        fontWeight="500"
      >
        Background Evictor
      </text>
      <text x="470" y="240" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        Weighted LRU · high/low water
      </text>

      {/* Metrics */}
      <rect
        x="400"
        y="265"
        width="140"
        height="45"
        rx="6"
        fill={MUTED}
        stroke={BORDER}
        strokeWidth="1"
      />
      <text
        x="470"
        y="287"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="10"
        fontWeight="500"
      >
        Prometheus Metrics
      </text>
      <text x="470" y="302" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        hits · misses · latency
      </text>

      {/* Arrow: Cache Service → Origin */}
      <line
        x1="580"
        y1="190"
        x2="635"
        y2="190"
        stroke={PRIMARY}
        strokeWidth="1.5"
        markerEnd="url(#arch-arr)"
      />
      <text x="607" y="183" textAnchor="middle" fill={MUTED_FG} fontSize="8">
        miss
      </text>

      {/* Right: Cloud Origin */}
      <rect
        x="640"
        y="130"
        width="130"
        height="120"
        rx="8"
        fill={MUTED}
        stroke={BORDER}
        strokeWidth="1.5"
      />
      <text
        x="705"
        y="160"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="12"
        fontWeight="600"
      >
        Cloud Origin
      </text>
      <text x="705" y="180" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        S3 · GCS · Azure
      </text>
      <text x="705" y="198" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        Source of truth
      </text>
      <text x="705" y="216" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        Immutable objects
      </text>
      <text x="705" y="234" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        xorbs · shards · packs
      </text>

      {/* Bottom labels */}
      <text x="80" y="370" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        CacheClient (reqwest)
      </text>
      <text x="390" y="370" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        axum + tower middleware
      </text>
      <text x="705" y="370" textAnchor="middle" fill={MUTED_FG} fontSize="9">
        object_store crate
      </text>
    </svg>
  )
}
