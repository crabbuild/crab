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
const CHECKPOINTS = ["v1", "v2", "v3", "v4"]

export function BinaryHistoryGrowthDiagram() {
  const heading = {
    fill: "var(--foreground)",
    fontFamily: "Inter, ui-sans-serif, system-ui",
    fontSize: 14,
    fontWeight: 650,
    textAnchor: "middle",
  } as const
  const detail = {
    fill: "var(--muted-foreground)",
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
    fontSize: 10,
    textAnchor: "middle",
  } as const

  return (
    <DiagramFrame
      title="Four checkpoints: full objects or unique chunks"
      caption="Illustrative model: each 8 GB checkpoint preserves 7.5 GB of encoded bytes from the previous version. The totals exclude compression and metadata overhead."
    >
      <svg
        viewBox="0 0 760 228"
        className="h-auto w-full min-w-[40rem]"
        role="img"
        aria-label="Four 8 gigabyte Git blobs total 32 gigabytes, while compact pointers and reusable chunks add 9.5 gigabytes of unique data to object storage"
      >
        <line
          x1="380"
          y1="18"
          x2="380"
          y2="210"
          stroke="var(--border)"
          strokeDasharray="4 5"
        />
        <text x="190" y="30" {...heading}>
          Ordinary Git blobs
        </text>
        <text x="570" y="30" {...heading}>
          Crab pointer path
        </text>

        {CHECKPOINTS.map((version, index) => {
          const gitX = 36 + index * 80
          const pointerX = 430 + index * 82
          return (
            <g key={version}>
              <rect
                x={gitX}
                y="60"
                width="66"
                height="66"
                rx="9"
                fill="color-mix(in srgb, #f97316 10%, var(--card))"
                stroke="#f97316"
                strokeWidth="1.5"
              />
              <text x={gitX + 33} y="87" {...heading} fontSize="12">
                {version}
              </text>
              <text x={gitX + 33} y="106" {...detail}>
                8 GB
              </text>
              <rect
                x={pointerX}
                y="60"
                width="58"
                height="36"
                rx="7"
                fill="color-mix(in srgb, #f97316 10%, var(--card))"
                stroke="#f97316"
                strokeWidth="1.5"
              />
              <text x={pointerX + 29} y="83" {...heading} fontSize="11">
                {version}
              </text>
            </g>
          )
        })}

        <text x="190" y="162" {...heading} fontSize="15" fontWeight="700">
          32 GB reachable history
        </text>
        <text x="190" y="182" {...detail}>
          every version remains a complete Git object
        </text>

        <rect
          x="430"
          y="126"
          width="235"
          height="44"
          rx="8"
          fill="color-mix(in srgb, #06b6d4 12%, var(--card))"
          stroke="#06b6d4"
          strokeWidth="1.5"
        />
        <text x="547.5" y="152" {...heading} fontSize="12">
          8 GB reusable base
        </text>
        {[0, 1, 2].map((index) => (
          <rect
            key={index}
            x={671 + index * 18}
            y="126"
            width="14"
            height="44"
            rx="4"
            fill="color-mix(in srgb, #f59e0b 12%, var(--card))"
            stroke="#f59e0b"
            strokeWidth="1.5"
          />
        ))}
        <text x="570" y="194" {...heading} fontSize="13" fontWeight="700">
          9.5 GB unique data + compact Git pointers
        </text>
        <text x="570" y="212" {...detail}>
          8 GB base + 3 × 0.5 GB new content
        </text>
      </svg>
    </DiagramFrame>
  )
}

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
