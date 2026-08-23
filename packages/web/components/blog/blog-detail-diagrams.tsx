import { DiagramFrame } from "@/components/blog/blog-diagrams"

const REUSED = {
  fill: "color-mix(in srgb, #10b981 12%, var(--card))",
  stroke: "#10b981",
}

const CHANGED = {
  fill: "color-mix(in srgb, #f59e0b 12%, var(--card))",
  stroke: "#f59e0b",
}

const VERSION_A = ["header", "weights 1", "weights 2", "optimizer", "footer"]
const VERSION_B = ["header", "weights 1", "new weights", "optimizer", "footer"]

export function ChunkReuseDiagram() {
  const startX = 126
  const segmentWidth = 112
  const gap = 6

  return (
    <DiagramFrame
      title="One local edit preserves distant chunks"
      caption="The edited region receives a new hash. Once content-defined boundaries resynchronize, the optimizer and footer chunks retain their earlier identities."
    >
      <svg
        viewBox="0 0 760 238"
        className="h-auto w-full min-w-[40rem]"
        role="img"
        aria-label="Two file versions with four reused regions and one changed region"
      >
        {[VERSION_A, VERSION_B].map((segments, rowIndex) => {
          const y = 38 + rowIndex * 76
          return (
            <g key={rowIndex}>
              <text
                x="26"
                y={y + 30}
                fill="var(--foreground)"
                fontFamily="Inter, ui-sans-serif, system-ui"
                fontSize="13"
                fontWeight="650"
              >
                Version {rowIndex === 0 ? "A" : "B"}
              </text>
              {segments.map((segment, index) => {
                const changed = index === 2
                const colors = changed ? CHANGED : REUSED
                const x = startX + index * (segmentWidth + gap)
                return (
                  <g key={segment}>
                    <rect
                      x={x}
                      y={y}
                      width={segmentWidth}
                      height="52"
                      rx="8"
                      fill={colors.fill}
                      stroke={colors.stroke}
                      strokeWidth="1.5"
                    />
                    <text
                      x={x + segmentWidth / 2}
                      y={y + 23}
                      textAnchor="middle"
                      fill="var(--foreground)"
                      fontFamily="Inter, ui-sans-serif, system-ui"
                      fontSize="11"
                      fontWeight="600"
                    >
                      {segment}
                    </text>
                    <text
                      x={x + segmentWidth / 2}
                      y={y + 39}
                      textAnchor="middle"
                      fill="var(--muted-foreground)"
                      fontFamily="ui-monospace, SFMono-Regular, Menlo, monospace"
                      fontSize="9"
                    >
                      {changed
                        ? rowIndex === 0
                          ? "old hash"
                          : "new hash"
                        : "same hash"}
                    </text>
                  </g>
                )
              })}
            </g>
          )
        })}
        <g transform="translate(126 208)">
          <rect
            width="11"
            height="11"
            rx="2"
            fill={REUSED.fill}
            stroke={REUSED.stroke}
          />
          <text x="18" y="9" fill="var(--muted-foreground)" fontSize="10">
            reused without upload
          </text>
          <rect
            x="178"
            width="11"
            height="11"
            rx="2"
            fill={CHANGED.fill}
            stroke={CHANGED.stroke}
          />
          <text x="196" y="9" fill="var(--muted-foreground)" fontSize="10">
            new chunk data
          </text>
        </g>
      </svg>
    </DiagramFrame>
  )
}

function ReachabilityMarker() {
  return (
    <marker
      id="gc-reachability-arrow"
      viewBox="0 0 10 10"
      refX="9"
      refY="5"
      markerWidth="8"
      markerHeight="8"
      orient="auto"
    >
      <path d="M1 1 9 5 1 9 3.5 5Z" fill="#64748b" />
    </marker>
  )
}

export function GarbageCollectionReachabilityDiagram() {
  const connector = {
    fill: "none",
    stroke: "#64748b",
    strokeWidth: 1.5,
    markerEnd: "url(#gc-reachability-arrow)",
  } as const

  return (
    <DiagramFrame
      title="Reachability and age decide each object outcome"
      caption="The collector retains every marked object. An unmarked object must also outlive the grace window before it becomes a deletion candidate."
    >
      <svg
        viewBox="0 0 760 334"
        className="h-auto w-full min-w-[40rem]"
        role="img"
        aria-label="Garbage collection combines retained roots and an object inventory before classifying objects"
      >
        <defs>
          <ReachabilityMarker />
        </defs>

        <path d="M175 104 C175 130 330 120 330 145" {...connector} />
        <path d="M585 104 C585 130 430 120 430 145" {...connector} />
        <path d="M330 217 C330 241 114 234 114 260" {...connector} />
        <path d="M380 217 V260" {...connector} />
        <path d="M430 217 C430 241 646 234 646 260" {...connector} />

        <g>
          <rect
            x="40"
            y="34"
            width="270"
            height="70"
            rx="10"
            fill="color-mix(in srgb, #8b5cf6 10%, var(--card))"
            stroke="#8b5cf6"
            strokeWidth="1.5"
          />
          <text
            x="175"
            y="63"
            textAnchor="middle"
            fill="var(--foreground)"
            fontSize="13"
            fontWeight="650"
          >
            Retained roots
          </text>
          <text
            x="175"
            y="83"
            textAnchor="middle"
            fill="var(--muted-foreground)"
            fontSize="10"
          >
            refs · recovery · workflows · holds
          </text>
        </g>

        <g>
          <rect
            x="450"
            y="34"
            width="270"
            height="70"
            rx="10"
            fill="color-mix(in srgb, #0284c7 10%, var(--card))"
            stroke="#0284c7"
            strokeWidth="1.5"
          />
          <text
            x="585"
            y="63"
            textAnchor="middle"
            fill="var(--foreground)"
            fontSize="13"
            fontWeight="650"
          >
            Object inventory snapshot
          </text>
          <text
            x="585"
            y="83"
            textAnchor="middle"
            fill="var(--muted-foreground)"
            fontSize="10"
          >
            packs · shards · xorbs
          </text>
        </g>

        <g>
          <rect
            x="250"
            y="145"
            width="260"
            height="72"
            rx="10"
            fill="color-mix(in srgb, #06b6d4 10%, var(--card))"
            stroke="#06b6d4"
            strokeWidth="1.5"
          />
          <text
            x="380"
            y="175"
            textAnchor="middle"
            fill="var(--foreground)"
            fontSize="13"
            fontWeight="650"
          >
            Mark dependencies and classify
          </text>
          <text
            x="380"
            y="196"
            textAnchor="middle"
            fill="var(--muted-foreground)"
            fontSize="10"
          >
            union roots · subtract live set · apply age
          </text>
        </g>

        {[
          { x: 24, title: "Reachable", detail: "retain", stroke: "#10b981" },
          {
            x: 290,
            title: "Recent orphan",
            detail: "wait for grace",
            stroke: "#f59e0b",
          },
          {
            x: 556,
            title: "Old orphan",
            detail: "eligible to delete",
            stroke: "#fb7185",
          },
        ].map((outcome) => (
          <g key={outcome.title}>
            <rect
              x={outcome.x}
              y="260"
              width="180"
              height="54"
              rx="9"
              fill={`color-mix(in srgb, ${outcome.stroke} 10%, var(--card))`}
              stroke={outcome.stroke}
              strokeWidth="1.5"
            />
            <text
              x={outcome.x + 90}
              y="283"
              textAnchor="middle"
              fill="var(--foreground)"
              fontSize="12"
              fontWeight="650"
            >
              {outcome.title}
            </text>
            <text
              x={outcome.x + 90}
              y="300"
              textAnchor="middle"
              fill="var(--muted-foreground)"
              fontSize="10"
            >
              {outcome.detail}
            </text>
          </g>
        ))}
      </svg>
    </DiagramFrame>
  )
}
