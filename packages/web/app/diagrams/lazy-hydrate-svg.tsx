/**
 * Lazy-Hydrate Slice Diagram
 *
 * Visualizes how a single oversized binary (e.g. a 200 GB CT scan, geospatial
 * tile, or CAD assembly) is sliced into xorbs in object storage and only
 * the slices touched by the current task are pulled to the workstation.
 *
 * Layout:
 *   Top: a wide bar representing the full file, partitioned into 20 xorb
 *        segments. A few segments are highlighted (sky) — those are the
 *        slices the current task needs.
 *   Bottom-left: workstation icon + "what's local"
 *   Bottom-right: object storage cylinder + "what's remote"
 *   Arrows pull only the highlighted segments down to the workstation.
 *
 * Server Component — uses CSS custom properties for theme adaptation.
 */

const PRIMARY = "var(--primary)"
const PRIMARY_MUTED = "var(--primary-muted)"
const FOREGROUND = "var(--foreground)"
const MUTED_FG = "var(--muted-foreground)"
const BORDER = "var(--border)"
const MUTED = "var(--muted)"
const CARD = "var(--card)"

const TOTAL_XORBS = 20
const HYDRATED_INDICES = new Set([3, 4, 11, 12, 13])

function ArrMarker({ id, color }: { id: string; color: string }) {
  return (
    <marker
      id={id}
      markerWidth="7"
      markerHeight="5"
      refX="7"
      refY="2.5"
      orient="auto"
    >
      <path d="M0 0L7 2.5L0 5" fill="none" stroke={color} strokeWidth="1.5" />
    </marker>
  )
}

export function LazyHydrateSvg() {
  const width = 760
  const height = 320

  const fileBarX = 30
  const fileBarY = 70
  const fileBarWidth = width - 60
  const fileBarHeight = 50
  const xorbWidth = fileBarWidth / TOTAL_XORBS

  const localBoxX = 60
  const localBoxY = 220
  const localBoxW = 240
  const localBoxH = 70

  const cloudBoxX = width - 60 - 240
  const cloudBoxY = 220
  const cloudBoxW = 240
  const cloudBoxH = 70

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      className="w-full h-auto"
      role="img"
      aria-label="Lazy hydrate diagram. A 200 gigabyte file is stored as 20 xorbs in object storage. Only 5 highlighted xorbs — the slices needed by the current task — are pulled down to the workstation."
    >
      <defs>
        <ArrMarker id="lh-arr-down" color={PRIMARY} />
      </defs>

      {/* Title */}
      <text
        x={width / 2}
        y="28"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="14"
        fontWeight="600"
      >
        200 GB on the bucket. Hydrate only the slices you read.
      </text>

      {/* File label */}
      <text x={fileBarX} y={fileBarY - 14} fill={FOREGROUND} fontSize="11" fontWeight="700">
        scan_volume.nii — 200 GB · stored as xorbs
      </text>
      <text
        x={fileBarX + fileBarWidth}
        y={fileBarY - 14}
        textAnchor="end"
        fill={MUTED_FG}
        fontSize="10"
      >
        compressed chunk packs
      </text>

      {/* File bar — segmented xorbs */}
      <g>
        {Array.from({ length: TOTAL_XORBS }).map((_, i) => {
          const x = fileBarX + i * xorbWidth
          const isHydrated = HYDRATED_INDICES.has(i)
          return (
            <rect
              key={i}
              x={x + 1}
              y={fileBarY}
              width={xorbWidth - 2}
              height={fileBarHeight}
              rx="3"
              fill={isHydrated ? PRIMARY : MUTED}
              stroke={isHydrated ? PRIMARY : BORDER}
              strokeWidth="1"
              opacity={isHydrated ? 1 : 0.6}
            />
          )
        })}
      </g>

      {/* Annotation under hydrated chunks */}
      {Array.from(HYDRATED_INDICES).map((idx) => {
        const cx = fileBarX + idx * xorbWidth + xorbWidth / 2
        return (
          <line
            key={`tick-${idx}`}
            x1={cx}
            y1={fileBarY + fileBarHeight}
            x2={cx}
            y2={fileBarY + fileBarHeight + 8}
            stroke={PRIMARY}
            strokeWidth="1.2"
          />
        )
      })}

      <text
        x={fileBarX + fileBarWidth / 2}
        y={fileBarY + fileBarHeight + 28}
        textAnchor="middle"
        fill={PRIMARY}
        fontSize="11"
        fontWeight="700"
      >
        ↓ 5 xorbs · ~12 GB pulled
      </text>
      <text
        x={fileBarX + fileBarWidth / 2}
        y={fileBarY + fileBarHeight + 44}
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="10"
      >
        the rest stays on object storage until referenced
      </text>

      {/* Local workstation card */}
      <rect
        x={localBoxX}
        y={localBoxY}
        width={localBoxW}
        height={localBoxH}
        rx="10"
        fill={PRIMARY_MUTED}
        stroke={PRIMARY}
        strokeWidth="1.5"
        opacity="0.6"
      />
      <text
        x={localBoxX + 16}
        y={localBoxY + 24}
        fill={FOREGROUND}
        fontSize="12"
        fontWeight="700"
      >
        Workstation
      </text>
      <text
        x={localBoxX + 16}
        y={localBoxY + 42}
        fill={MUTED_FG}
        fontSize="10"
      >
        Hydrated slices · 12 GB
      </text>
      <text
        x={localBoxX + 16}
        y={localBoxY + 58}
        fill={MUTED_FG}
        fontSize="10"
      >
        Pointer blob · everything else
      </text>

      {/* Cloud / object storage card */}
      <rect
        x={cloudBoxX}
        y={cloudBoxY}
        width={cloudBoxW}
        height={cloudBoxH}
        rx="10"
        fill={CARD}
        stroke={BORDER}
        strokeWidth="1.5"
      />
      <text
        x={cloudBoxX + 16}
        y={cloudBoxY + 24}
        fill={FOREGROUND}
        fontSize="12"
        fontWeight="700"
      >
        Object Storage
      </text>
      <text
        x={cloudBoxX + 16}
        y={cloudBoxY + 42}
        fill={MUTED_FG}
        fontSize="10"
      >
        20 xorbs · S3 · GCS · Azure
      </text>
      <text
        x={cloudBoxX + 16}
        y={cloudBoxY + 58}
        fill={MUTED_FG}
        fontSize="10"
      >
        Blake3-verified · resumable
      </text>

      {/* Pull arrow — cloud to workstation (hydrate direction) */}
      <line
        x1={cloudBoxX}
        y1={cloudBoxY + cloudBoxH / 2}
        x2={localBoxX + localBoxW + 4}
        y2={localBoxY + localBoxH / 2}
        stroke={PRIMARY}
        strokeWidth="1.5"
        markerEnd="url(#lh-arr-down)"
      />
      <text
        x={(cloudBoxX + localBoxX + localBoxW) / 2}
        y={cloudBoxY + cloudBoxH / 2 - 8}
        textAnchor="middle"
        fill={PRIMARY}
        fontSize="10"
        fontWeight="600"
      >
        crab hydrate slice.nii
      </text>
      <text
        x={(cloudBoxX + localBoxX + localBoxW) / 2}
        y={cloudBoxY + cloudBoxH / 2 + 14}
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        only the 5 highlighted xorbs cross the wire
      </text>
    </svg>
  )
}
