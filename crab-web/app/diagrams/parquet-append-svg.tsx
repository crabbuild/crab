/**
 * Append-Only Parquet Dedup Diagram
 *
 * Visualizes how content-defined chunking handles append-only growth: a
 * 50 GB Parquet file gains a million rows and only the new chunks at the
 * tail upload. The bulk of the file is reused chunk-for-chunk.
 *
 * Used on the Data Science use-case section.
 *
 * Server Component — uses CSS custom properties for theme adaptation.
 */

const PRIMARY = "var(--primary)"
const PRIMARY_MUTED = "var(--primary-muted)"
const FOREGROUND = "var(--foreground)"
const MUTED_FG = "var(--muted-foreground)"
const BORDER = "var(--border)"
const MUTED = "var(--muted)"

interface FileSpec {
  label: string
  sub: string
  /** Total bar width as a fraction of the available width (0-1). */
  width: number
  /** Trailing fraction (0-1) that is "new" (sky highlight). */
  newFraction: number
  /** Right-side metric. */
  metric: string
  metricSub: string
}

const files: FileSpec[] = [
  {
    label: "v1 — last week",
    sub: "50 GB Parquet",
    width: 0.85,
    newFraction: 1,
    metric: "50 GB uploaded",
    metricSub: "first commit",
  },
  {
    label: "v2 — today",
    sub: "+1M rows appended",
    width: 0.93,
    newFraction: 0.08,
    metric: "4 GB uploaded",
    metricSub: "92% chunks dedup'd",
  },
]

export function ParquetAppendSvg() {
  const width = 760
  const height = 220
  const labelX = 16
  const barX = 196
  const barEndX = 540
  const barFullWidth = barEndX - barX
  const metricX = barEndX + 28
  const barHeight = 44

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      className="w-full h-auto"
      role="img"
      aria-label="Append-only dedup diagram. v1 of a 50 gigabyte Parquet file uploads in full. v2, with one million additional rows appended, only re-uploads the trailing chunks — about 4 gigabytes — because the rest of the file is byte-identical."
    >
      {/* Title */}
      <text
        x={width / 2}
        y="28"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="14"
        fontWeight="600"
        letterSpacing="0.02em"
      >
        Append a million rows. Upload only the new chunks.
      </text>

      {/* Legend */}
      <g transform="translate(16 46)">
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

      {files.map((file, i) => {
        const y = 80 + i * 70
        const barWidth = barFullWidth * file.width
        const newWidth = barWidth * file.newFraction
        const reusedWidth = barWidth - newWidth

        return (
          <g key={file.label}>
            {/* Row label */}
            <text
              x={labelX}
              y={y + 18}
              fill={FOREGROUND}
              fontSize="12"
              fontWeight="700"
            >
              {file.label}
            </text>
            <text x={labelX} y={y + 34} fill={MUTED_FG} fontSize="10">
              {file.sub}
            </text>

            {/* Reused (existing) portion */}
            {reusedWidth > 0 && (
              <rect
                x={barX}
                y={y}
                width={reusedWidth}
                height={barHeight}
                rx="6"
                fill={MUTED}
                stroke={BORDER}
                strokeWidth="1"
                opacity={i === 0 ? 0 : 0.7}
              />
            )}
            {/* For v1 we tint the whole thing as "new" */}
            {i === 0 && (
              <rect
                x={barX}
                y={y}
                width={barWidth}
                height={barHeight}
                rx="6"
                fill={PRIMARY_MUTED}
                stroke={PRIMARY}
                strokeWidth="1"
                opacity="0.5"
              />
            )}
            {/* New chunks */}
            <rect
              x={i === 0 ? barX : barX + reusedWidth}
              y={y}
              width={i === 0 ? barWidth : newWidth}
              height={barHeight}
              rx="6"
              fill={PRIMARY}
              opacity={i === 0 ? 0.85 : 1}
            />

            {/* Inline labels inside the bar segments */}
            {i === 0 ? (
              <text
                x={barX + barWidth / 2}
                y={y + 27}
                textAnchor="middle"
                fill={"var(--primary-foreground)"}
                fontSize="10"
                fontWeight="700"
              >
                50 GB · 16,000 chunks
              </text>
            ) : (
              <>
                <text
                  x={barX + reusedWidth / 2}
                  y={y + 27}
                  textAnchor="middle"
                  fill={MUTED_FG}
                  fontSize="10"
                  fontWeight="600"
                >
                  reused
                </text>
                <text
                  x={barX + reusedWidth + newWidth / 2}
                  y={y + 27}
                  textAnchor="middle"
                  fill={"var(--primary-foreground)"}
                  fontSize="9"
                  fontWeight="700"
                >
                  new
                </text>
              </>
            )}

            {/* Metric */}
            <text
              x={metricX}
              y={y + 18}
              fill={i === 0 ? FOREGROUND : PRIMARY}
              fontSize="14"
              fontWeight="800"
            >
              {file.metric}
            </text>
            <text x={metricX} y={y + 34} fill={MUTED_FG} fontSize="10">
              {file.metricSub}
            </text>
          </g>
        )
      })}

      {/* Footer caption */}
      <text
        x={width / 2}
        y={height - 14}
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="10"
      >
        Gearhash CDC keeps chunk boundaries stable across appends — only the tail moves.
      </text>
    </svg>
  )
}
