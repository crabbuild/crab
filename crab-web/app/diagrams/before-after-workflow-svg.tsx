/**
 * Before/After Workflow Comparison
 *
 * Visualizes the difference between a traditional Git LFS workflow and the
 * Crab workflow for the same large-asset repository. The "before" row shows
 * the developer waiting on a full LFS pull of every tracked binary; the
 * "after" row shows Crab's lazy checkout pulling only the chunks that the
 * current task actually needs.
 *
 * Each row carries quantitative metric annotations (data pulled, wall-clock
 * time) so a reader can scan the comparison without reading the surrounding
 * prose.
 *
 * Server Component — uses CSS custom properties so it adapts to dark and
 * light mode automatically without any client JavaScript.
 */

const PRIMARY = "var(--primary)"
const PRIMARY_MUTED = "var(--primary-muted)"
const FOREGROUND = "var(--foreground)"
const MUTED_FG = "var(--muted-foreground)"
const BORDER = "var(--border)"
const MUTED = "var(--muted)"
const CARD = "var(--card)"

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
      <path
        d="M0 0L7 2.5L0 5"
        fill="none"
        stroke={color}
        strokeWidth="1.5"
      />
    </marker>
  )
}

interface Step {
  /** Label on the box, e.g. "git clone". */
  label: string
  /** Optional sub-label rendered in a smaller muted font. */
  sub?: string
}

interface RowProps {
  y: number
  /** Title rendered to the left of the row, e.g. "Traditional Git LFS". */
  title: string
  /** Single-sentence subtitle under the title. */
  subtitle: string
  steps: Step[]
  /** Strong-emphasis metric, e.g. "80 GB pulled". */
  metricPrimary: string
  /** Secondary metric, e.g. "12 min". */
  metricSecondary: string
  /** Visual treatment — `crab` highlights the row with sky accents. */
  variant: "lfs" | "crab"
}

function WorkflowRow({
  y,
  title,
  subtitle,
  steps,
  metricPrimary,
  metricSecondary,
  variant,
}: RowProps) {
  // Layout constants — keep the two rows on the same horizontal grid so steps
  // line up vertically between "before" and "after".
  const labelX = 16
  const stepStartX = 196
  const stepWidth = 120
  const stepHeight = 52
  const stepGap = 24
  const stepY = y + 24
  const metricX = stepStartX + steps.length * stepWidth + (steps.length - 1) * stepGap + 24
  const arrowColor = variant === "crab" ? PRIMARY : BORDER
  const stepStroke = variant === "crab" ? PRIMARY : BORDER
  const stepFill = variant === "crab" ? PRIMARY_MUTED : CARD
  const stepLabelColor = variant === "crab" ? PRIMARY : FOREGROUND
  const arrowMarker =
    variant === "crab" ? "url(#ba-arr-crab)" : "url(#ba-arr-lfs)"

  return (
    <g>
      {/* Row label (left column) */}
      <text
        x={labelX}
        y={y + 26}
        fill={FOREGROUND}
        fontSize="13"
        fontWeight="700"
        letterSpacing="0.02em"
      >
        {title}
      </text>
      <text
        x={labelX}
        y={y + 46}
        fill={MUTED_FG}
        fontSize="10"
      >
        {subtitle}
      </text>

      {/* Background guide rail under the steps */}
      <line
        x1={stepStartX}
        y1={stepY + stepHeight / 2}
        x2={metricX - 16}
        y2={stepY + stepHeight / 2}
        stroke={MUTED}
        strokeWidth="1"
        strokeDasharray="2 4"
        opacity="0.6"
      />

      {/* Step boxes + arrows */}
      {steps.map((step, i) => {
        const x = stepStartX + i * (stepWidth + stepGap)
        const centerX = x + stepWidth / 2

        return (
          <g key={`${title}-${step.label}`}>
            <rect
              x={x}
              y={stepY}
              width={stepWidth}
              height={stepHeight}
              rx="8"
              fill={stepFill}
              stroke={stepStroke}
              strokeWidth="1.5"
            />
            <text
              x={centerX}
              y={stepY + 22}
              textAnchor="middle"
              fill={stepLabelColor}
              fontSize="11"
              fontWeight="700"
            >
              {step.label}
            </text>
            {step.sub ? (
              <text
                x={centerX}
                y={stepY + 38}
                textAnchor="middle"
                fill={MUTED_FG}
                fontSize="9"
              >
                {step.sub}
              </text>
            ) : null}

            {i < steps.length - 1 ? (
              <line
                x1={x + stepWidth}
                y1={stepY + stepHeight / 2}
                x2={x + stepWidth + stepGap}
                y2={stepY + stepHeight / 2}
                stroke={arrowColor}
                strokeWidth="1.5"
                markerEnd={arrowMarker}
              />
            ) : null}
          </g>
        )
      })}

      {/* Metric annotations on the right */}
      <text
        x={metricX}
        y={stepY + 22}
        fill={variant === "crab" ? PRIMARY : FOREGROUND}
        fontSize="16"
        fontWeight="800"
        letterSpacing="0.01em"
      >
        {metricPrimary}
      </text>
      <text
        x={metricX}
        y={stepY + 40}
        fill={MUTED_FG}
        fontSize="10"
      >
        {metricSecondary}
      </text>

      {/* Optional row-end accent dot for the Crab row */}
      {variant === "crab" ? (
        <circle
          cx={metricX - 8}
          cy={stepY + stepHeight / 2}
          r="3"
          fill={PRIMARY}
        />
      ) : null}
    </g>
  )
}

export function BeforeAfterWorkflowSvg() {
  // Traditional Git LFS: clone the repo, then pull every tracked binary in
  // full before any work can start.
  const lfsSteps: Step[] = [
    { label: "git clone", sub: "+ LFS hooks" },
    { label: "git lfs pull", sub: "all binaries" },
    { label: "ready", sub: "full working tree" },
  ]

  // Crab: clone is fast because objects are pointer blobs; lazy checkout
  // (or FUSE) materializes only the chunks the current task touches.
  const crabSteps: Step[] = [
    { label: "git clone", sub: "crab:// remote" },
    { label: "lazy checkout", sub: "pointer blobs" },
    { label: "crab hydrate", sub: "needed chunks" },
  ]

  // viewBox sized for 3 steps + label column + metric column.
  const width = 760
  const height = 280

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      role="img"
      aria-label="Workflow comparison diagram. Top row: traditional Git LFS clones the repo, pulls 80 gigabytes of binaries, takes 12 minutes before the developer can work. Bottom row: Crab clones with lazy checkout, hydrates only needed chunks, pulls 2.3 gigabytes in 30 seconds."
      className="w-full h-auto"
    >
      <defs>
        <ArrMarker id="ba-arr-lfs" color={BORDER} />
        <ArrMarker id="ba-arr-crab" color={PRIMARY} />
      </defs>

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
        Same repository, two workflows
      </text>

      {/* Before row */}
      <WorkflowRow
        y={56}
        title="Git LFS"
        subtitle="Pulls every tracked binary up front"
        steps={lfsSteps}
        metricPrimary="80 GB pulled"
        metricSecondary="≈ 12 min before first edit"
        variant="lfs"
      />

      {/* Divider */}
      <line
        x1="16"
        y1={height / 2 + 16}
        x2={width - 16}
        y2={height / 2 + 16}
        stroke={BORDER}
        strokeWidth="1"
        strokeDasharray="3 4"
        opacity="0.6"
      />

      {/* After row */}
      <WorkflowRow
        y={height / 2 + 32}
        title="Crab"
        subtitle="Lazy checkout, on-demand hydrate"
        steps={crabSteps}
        metricPrimary="2.3 GB pulled"
        metricSecondary="≈ 30 sec before first edit"
        variant="crab"
      />

      {/* Footer caption */}
      <text
        x={width / 2}
        y={height - 10}
        textAnchor="middle"
        fill={MUTED_FG}
        fontSize="10"
      >
        Metrics from a 200 GB game-asset repo; only the textures and meshes touched by one task are hydrated.
      </text>
    </svg>
  )
}
