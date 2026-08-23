import { DiagramFrame } from "@/components/blog/blog-diagrams"

type DocsPathTone = "git" | "data" | "control" | "store" | "safe" | "warning"

type DocsPathStep = {
  label: string
  detail: string
  tone: DocsPathTone
}

const TONE: Record<DocsPathTone, { fill: string; stroke: string }> = {
  git: {
    fill: "color-mix(in srgb, #f97316 10%, var(--card))",
    stroke: "#f97316",
  },
  data: {
    fill: "color-mix(in srgb, #06b6d4 10%, var(--card))",
    stroke: "#06b6d4",
  },
  control: {
    fill: "color-mix(in srgb, #8b5cf6 10%, var(--card))",
    stroke: "#8b5cf6",
  },
  store: {
    fill: "color-mix(in srgb, #0284c7 10%, var(--card))",
    stroke: "#0284c7",
  },
  safe: {
    fill: "color-mix(in srgb, #10b981 10%, var(--card))",
    stroke: "#10b981",
  },
  warning: {
    fill: "color-mix(in srgb, #f59e0b 10%, var(--card))",
    stroke: "#f59e0b",
  },
}

export function DocsPathDiagram({
  id,
  title,
  caption,
  steps,
}: {
  id: string
  title: string
  caption: string
  steps: DocsPathStep[]
}) {
  const boxWidth = 146
  const gap = 50
  const padding = 22
  const width = padding * 2 + steps.length * boxWidth + (steps.length - 1) * gap

  return (
    <DiagramFrame title={title} caption={caption}>
      <svg
        viewBox={`0 0 ${width} 176`}
        className="h-auto w-full"
        style={{ minWidth: `${Math.max(width, 640)}px` }}
        role="img"
        aria-label={`${title}: ${steps.map((step) => step.label).join(" then ")}`}
      >
        <defs>
          <marker
            id={`${id}-arrow`}
            viewBox="0 0 10 10"
            refX="9"
            refY="5"
            markerWidth="8"
            markerHeight="8"
            orient="auto"
          >
            <path d="M1 1 9 5 1 9 3.5 5Z" fill="#64748b" />
          </marker>
        </defs>

        {steps.slice(0, -1).map((step, index) => {
          const x = padding + index * (boxWidth + gap)
          return (
            <line
              key={`${step.label}-connector`}
              x1={x + boxWidth}
              y1="70"
              x2={x + boxWidth + gap}
              y2="70"
              stroke="#64748b"
              strokeWidth="1.5"
              markerEnd={`url(#${id}-arrow)`}
            />
          )
        })}

        {steps.map((step, index) => {
          const x = padding + index * (boxWidth + gap)
          const colors = TONE[step.tone]
          return (
            <g key={step.label}>
              <rect
                x={x}
                y="34"
                width={boxWidth}
                height="72"
                rx="10"
                fill="var(--card)"
              />
              <rect
                x={x}
                y="34"
                width={boxWidth}
                height="72"
                rx="10"
                fill={colors.fill}
                stroke={colors.stroke}
                strokeWidth="1.5"
              />
              <text
                x={x + boxWidth / 2}
                y="63"
                textAnchor="middle"
                fill="var(--foreground)"
                fontFamily="Inter, ui-sans-serif, system-ui"
                fontSize="12"
                fontWeight="650"
              >
                {step.label}
              </text>
              <text
                x={x + boxWidth / 2}
                y="84"
                textAnchor="middle"
                fill="var(--muted-foreground)"
                fontFamily="ui-monospace, SFMono-Regular, Menlo, monospace"
                fontSize="9"
              >
                {step.detail}
              </text>
              <circle
                cx={x + boxWidth / 2}
                cy="132"
                r="12"
                fill="var(--card)"
                stroke={colors.stroke}
                strokeWidth="1.5"
              />
              <text
                x={x + boxWidth / 2}
                y="136"
                textAnchor="middle"
                fill="var(--foreground)"
                fontSize="10"
                fontWeight="650"
              >
                {index + 1}
              </text>
            </g>
          )
        })}
      </svg>
    </DiagramFrame>
  )
}
