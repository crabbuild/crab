/**
 * Cost Breakdown Diagram
 *
 * Visualizes the three components of cloud storage cost:
 *   Storage (60%) | Operations (10%) | Egress (30%)
 *
 * Rendered as three labeled boxes with proportional widths showing
 * a typical cost breakdown for Crab usage.
 *
 * Server Component — uses CSS custom properties so the SVG adapts to dark
 * and light mode automatically without any client JavaScript.
 */

const PRIMARY = "var(--primary)"
const FOREGROUND = "var(--foreground)"
const MUTED_FG = "var(--muted-foreground)"
const BORDER = "var(--border)"
const MUTED = "var(--muted)"
const CARD = "var(--card)"

export function CostBreakdownSvg() {
  // Proportional widths for the three cost components
  // Total usable width: 600 (with 10px margins on each side = 620 viewBox)
  // Storage: 60% = 360px, Operations: 10% = 60px, Egress: 30% = 180px
  // Gaps between boxes: 2 × 10px = 20px total
  // Adjusted: Storage 348, Ops 54, Egress 168 (with 2 × 15px gaps)

  const barY = 70
  const barHeight = 80
  const startX = 20

  // Storage: 60%
  const storageW = 348
  const storageX = startX

  // Operations: 10%
  const opsW = 60
  const opsX = storageX + storageW + 15

  // Egress: 30%
  const egressW = 177
  const egressX = opsX + opsW + 15

  return (
    <svg
      viewBox="0 0 640 200"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      role="img"
      aria-label="Cost breakdown: Storage 60%, Operations 10%, Egress 30%"
      width="100%"
      className="h-auto"
    >
      {/* Title */}
      <text
        x="320"
        y="30"
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="13"
        fontWeight="600"
      >
        Monthly Cost Breakdown
      </text>

      {/* Subtitle */}
      <text
        x="320"
        y="50"
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="10"
      >
        Typical distribution for a 100 GB repository
      </text>

      {/* Storage box — 60% */}
      <rect
        x={storageX}
        y={barY}
        width={storageW}
        height={barHeight}
        rx="8"
        fill={CARD}
        stroke={PRIMARY}
        strokeWidth="1.5"
      />
      <text
        x={storageX + storageW / 2}
        y={barY + 30}
        textAnchor="middle"
        fill={PRIMARY}
        fontSize="12"
        fontWeight="700"
      >
        Storage
      </text>
      <text
        x={storageX + storageW / 2}
        y={barY + 48}
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        $/GB/month
      </text>
      <text
        x={storageX + storageW / 2}
        y={barY + 66}
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="11"
        fontWeight="600"
      >
        ~60%
      </text>

      {/* Operations box — 10% */}
      <rect
        x={opsX}
        y={barY}
        width={opsW}
        height={barHeight}
        rx="8"
        fill={MUTED}
        stroke={BORDER}
        strokeWidth="1.5"
      />
      <text
        x={opsX + opsW / 2}
        y={barY + 30}
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="10"
        fontWeight="600"
      >
        Ops
      </text>
      <text
        x={opsX + opsW / 2}
        y={barY + 46}
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="8"
      >
        PUT/GET
      </text>
      <text
        x={opsX + opsW / 2}
        y={barY + 62}
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="10"
        fontWeight="600"
      >
        ~10%
      </text>

      {/* Egress box — 30% */}
      <rect
        x={egressX}
        y={barY}
        width={egressW}
        height={barHeight}
        rx="8"
        fill={CARD}
        stroke={PRIMARY}
        strokeWidth="1.5"
        strokeDasharray="4 2"
      />
      <text
        x={egressX + egressW / 2}
        y={barY + 30}
        textAnchor="middle"
        fill={PRIMARY}
        fontSize="12"
        fontWeight="700"
      >
        Egress
      </text>
      <text
        x={egressX + egressW / 2}
        y={barY + 48}
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="9"
      >
        $/GB downloaded
      </text>
      <text
        x={egressX + egressW / 2}
        y={barY + 66}
        textAnchor="middle"
        fill={FOREGROUND}
        fontSize="11"
        fontWeight="600"
      >
        ~30%
      </text>

      {/* Bottom annotations */}
      <text
        x={storageX + storageW / 2}
        y={barY + barHeight + 20}
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="8"
      >
        $0.023/GB (S3 Standard)
      </text>
      <text
        x={opsX + opsW / 2}
        y={barY + barHeight + 20}
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="8"
      >
        $0.005/1K
      </text>
      <text
        x={egressX + egressW / 2}
        y={barY + barHeight + 20}
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="8"
      >
        $0.09/GB (varies by region)
      </text>
    </svg>
  )
}
