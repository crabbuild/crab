/**
 * ML Fine-Tune Dedup Diagram
 *
 * Visualizes the most compelling property of Crab for ML teams: a fine-tune
 * that touches a few transformer layers re-uploads only the changed chunks,
 * not the whole multi-gigabyte checkpoint.
 *
 * Layout:
 *   Row 1 (Base model)        : N chunks, all sky-colored (initial upload)
 *   Row 2 (Fine-tune v1)      : Same chunks dedup'd (muted) + a few new (sky)
 *   Row 3 (Fine-tune v2)      : Same again, even fewer new chunks
 *   Right column              : Per-row "uploaded" metric badge
 *   Bottom caption            : "8 GB model · 80 MB delta" headline
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

interface Row {
  /** Title rendered above the chunk strip. */
  label: string
  /** Sub-label, e.g. branch name. */
  sub: string
  /** Chunk pattern: "new" = sky chunk uploaded, "dedup" = grey chunk reused. */
  chunks: Array<"new" | "dedup">
  /** Right-side metric, e.g. "8 GB uploaded". */
  metric: string
  /** Secondary metric line below the primary. */
  metricSub: string
}

const rows: Row[] = [
  {
    label: "Base model",
    sub: "v1 — initial commit",
    chunks: Array.from({ length: 16 }, () => "new"),
    metric: "8 GB uploaded",
    metricSub: "first push",
  },
  {
    label: "Fine-tune v2",
    sub: "adjusted last 3 layers",
    chunks: [
      "dedup", "dedup", "dedup", "dedup", "dedup", "dedup", "dedup", "dedup",
      "dedup", "dedup", "dedup", "dedup", "dedup", "new", "new", "new",
    ],
    metric: "150 MB uploaded",
    metricSub: "13 chunks dedup'd",
  },
  {
    label: "Fine-tune v3",
    sub: "single-layer LoRA",
    chunks: [
      "dedup", "dedup", "dedup", "dedup", "dedup", "dedup", "dedup", "dedup",
      "dedup", "dedup", "dedup", "dedup", "dedup", "dedup", "dedup", "new",
    ],
    metric: "50 MB uploaded",
    metricSub: "15 chunks dedup'd",
  },
]

const CHUNK_W = 24
const CHUNK_H = 22
const CHUNK_GAP = 4
const CHUNKS_PER_ROW = 16

function ChunkStrip({
  x,
  y,
  chunks,
}: {
  x: number
  y: number
  chunks: Array<"new" | "dedup">
}) {
  return (
    <g>
      {chunks.map((kind, i) => {
        const cx = x + i * (CHUNK_W + CHUNK_GAP)
        return (
          <rect
            key={i}
            x={cx}
            y={y}
            width={CHUNK_W}
            height={CHUNK_H}
            rx="3"
            fill={kind === "new" ? PRIMARY : MUTED}
            stroke={kind === "new" ? PRIMARY : BORDER}
            strokeWidth="1"
            opacity={kind === "new" ? 1 : 0.7}
          />
        )
      })}
    </g>
  )
}

export function UseCaseWorkflowSvg() {
  const labelX = 16
  const stripX = 196
  const stripWidthPx =
    CHUNKS_PER_ROW * CHUNK_W + (CHUNKS_PER_ROW - 1) * CHUNK_GAP
  const metricX = stripX + stripWidthPx + 28
  const rowGap = 86
  const rowStartY = 64
  const totalWidth = metricX + 150
  const totalHeight = rowStartY + rows.length * rowGap + 30

  return (
    <svg
      viewBox={`0 0 ${totalWidth} ${totalHeight}`}
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      className="w-full h-auto"
      role="img"
      aria-label="Fine-tune deduplication: an 8 gigabyte base model on the first push, with subsequent fine-tune versions uploading only 150 megabytes and then 50 megabytes thanks to chunk-level deduplication."
    >
      {/* Title */}
      <text
        x={totalWidth / 2}
        y="28"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="14"
        fontWeight="600"
        letterSpacing="0.02em"
      >
        Each chunk uploaded once. Fine-tunes reuse the rest.
      </text>

      {/* Legend */}
      <g transform={`translate(${labelX} 44)`}>
        <rect x="0" y="0" width="14" height="12" rx="2" fill={PRIMARY} />
        <text x="20" y="10" fill={MUTED_FG} fontSize="10">
          new chunk · uploaded
        </text>
        <rect
          x="180"
          y="0"
          width="14"
          height="12"
          rx="2"
          fill={MUTED}
          stroke={BORDER}
          strokeWidth="1"
          opacity="0.7"
        />
        <text x="200" y="10" fill={MUTED_FG} fontSize="10">
          existing · dedup&apos;d
        </text>
      </g>

      {rows.map((row, i) => {
        const y = rowStartY + i * rowGap
        return (
          <g key={row.label}>
            {/* Row label */}
            <text
              x={labelX}
              y={y + 18}
              fill={FOREGROUND}
              fontSize="12"
              fontWeight="700"
            >
              {row.label}
            </text>
            <text x={labelX} y={y + 34} fill={MUTED_FG} fontSize="10">
              {row.sub}
            </text>

            {/* Background card under chunk strip */}
            <rect
              x={stripX - 10}
              y={y + 4}
              width={stripWidthPx + 20}
              height={CHUNK_H + 16}
              rx="6"
              fill={i === 0 ? PRIMARY_MUTED : CARD}
              stroke={BORDER}
              strokeWidth="1"
              opacity={i === 0 ? 0.4 : 1}
            />

            <ChunkStrip x={stripX} y={y + 12} chunks={row.chunks} />

            {/* Per-row metric */}
            <text
              x={metricX}
              y={y + 18}
              fill={i === 0 ? FOREGROUND : PRIMARY}
              fontSize="14"
              fontWeight="800"
              letterSpacing="0.01em"
            >
              {row.metric}
            </text>
            <text x={metricX} y={y + 34} fill={MUTED_FG} fontSize="10">
              {row.metricSub}
            </text>
          </g>
        )
      })}

      {/* Footer caption */}
      <text
        x={totalWidth / 2}
        y={totalHeight - 8}
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="10"
      >
        Content-defined chunking + 3-tier dedup (session → shard → DB index).
      </text>
    </svg>
  )
}
