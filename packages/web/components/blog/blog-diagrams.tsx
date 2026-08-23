import type { ReactNode } from "react"

import { cn } from "@/lib/utils"

type Tone = "git" | "data" | "control" | "store" | "safe" | "muted"

const TONE: Record<Tone, { stroke: string; fill: string }> = {
  git: {
    stroke: "#f97316",
    fill: "color-mix(in srgb, #f97316 10%, var(--card))",
  },
  data: {
    stroke: "#06b6d4",
    fill: "color-mix(in srgb, #06b6d4 10%, var(--card))",
  },
  control: {
    stroke: "#8b5cf6",
    fill: "color-mix(in srgb, #8b5cf6 10%, var(--card))",
  },
  store: {
    stroke: "#0284c7",
    fill: "color-mix(in srgb, #0284c7 10%, var(--card))",
  },
  safe: {
    stroke: "#10b981",
    fill: "color-mix(in srgb, #10b981 10%, var(--card))",
  },
  muted: { stroke: "var(--border)", fill: "var(--muted)" },
}

type FlowItem = {
  label: string
  detail: string
  tone: Tone
}

export function DiagramFrame({
  title,
  caption,
  children,
  className,
}: {
  title: string
  caption: string
  children: ReactNode
  className?: string
}) {
  return (
    <figure
      className={cn(
        "not-prose my-10 overflow-hidden rounded-xl border border-border bg-card shadow-sm",
        className
      )}
    >
      <div className="border-b border-border bg-muted/30 px-5 py-4">
        <p className="m-0 text-sm font-semibold text-foreground">{title}</p>
      </div>
      <div
        className="overflow-x-auto p-4 focus-visible:ring-2 focus-visible:ring-sky-500 focus-visible:outline-none focus-visible:ring-inset sm:p-6"
        role="region"
        aria-label={`${title} diagram`}
        tabIndex={0}
      >
        <p className="m-0 mb-3 font-mono text-[10px] tracking-wide text-muted-foreground sm:hidden">
          Scroll horizontally to explore the full diagram →
        </p>
        {children}
      </div>
      <figcaption className="border-t border-border px-5 py-3 text-xs leading-5 text-muted-foreground">
        {caption}
      </figcaption>
    </figure>
  )
}

function ArrowMarker({
  id,
  color = "#64748b",
}: {
  id: string
  color?: string
}) {
  return (
    <marker
      id={id}
      viewBox="0 0 10 10"
      refX="9"
      refY="5"
      markerWidth="8"
      markerHeight="8"
      orient="auto-start-reverse"
    >
      <path d="M1 1 9 5 1 9 3.5 5Z" fill={color} />
    </marker>
  )
}

function FlowDiagram({
  id,
  label,
  items,
  footer,
}: {
  id: string
  label: string
  items: readonly FlowItem[]
  footer?: string
}) {
  const boxWidth = 142
  const gap = 44
  const startX = 18
  const width = startX * 2 + items.length * boxWidth + (items.length - 1) * gap

  return (
    <svg
      viewBox={`0 0 ${width} 178`}
      className="h-auto w-full min-w-[42rem]"
      role="img"
      aria-label={label}
    >
      <defs>
        <ArrowMarker id={`${id}-arrow`} />
      </defs>
      {items.map((item, index) => {
        const x = startX + index * (boxWidth + gap)
        const colors = TONE[item.tone]
        return (
          <g key={item.label}>
            {index < items.length - 1 ? (
              <line
                x1={x + boxWidth}
                y1="72"
                x2={x + boxWidth + gap}
                y2="72"
                stroke="#64748b"
                strokeWidth="1.5"
                markerEnd={`url(#${id}-arrow)`}
              />
            ) : null}
            <rect
              x={x}
              y="30"
              width={boxWidth}
              height="84"
              rx="10"
              fill={colors.fill}
              stroke={colors.stroke}
              strokeWidth="1.5"
            />
            <text
              x={x + boxWidth / 2}
              y="61"
              textAnchor="middle"
              fill="var(--foreground)"
              fontFamily="Inter, ui-sans-serif, system-ui"
              fontSize="13"
              fontWeight="650"
            >
              {item.label}
            </text>
            <text
              x={x + boxWidth / 2}
              y="84"
              textAnchor="middle"
              fill="var(--muted-foreground)"
              fontFamily="ui-monospace, SFMono-Regular, Menlo, monospace"
              fontSize="10"
            >
              {item.detail}
            </text>
          </g>
        )
      })}
      {footer ? (
        <text
          x={width / 2}
          y="153"
          textAnchor="middle"
          fill="var(--muted-foreground)"
          fontFamily="Inter, ui-sans-serif, system-ui"
          fontSize="11"
        >
          {footer}
        </text>
      ) : null}
    </svg>
  )
}

function CompareDiagram({
  id,
  label,
  leftTitle,
  leftItems,
  rightTitle,
  rightItems,
}: {
  id: string
  label: string
  leftTitle: string
  leftItems: readonly string[]
  rightTitle: string
  rightItems: readonly string[]
}) {
  const rows = Math.max(leftItems.length, rightItems.length)
  const height = 96 + rows * 42
  return (
    <svg
      viewBox={`0 0 760 ${height}`}
      className="h-auto w-full min-w-[40rem]"
      role="img"
      aria-label={label}
    >
      <defs>
        <ArrowMarker id={`${id}-arrow`} color="#0284c7" />
      </defs>
      <text
        x="170"
        y="28"
        textAnchor="middle"
        fill="var(--foreground)"
        fontSize="14"
        fontWeight="650"
      >
        {leftTitle}
      </text>
      <text
        x="590"
        y="28"
        textAnchor="middle"
        fill="var(--foreground)"
        fontSize="14"
        fontWeight="650"
      >
        {rightTitle}
      </text>
      <line
        x1="380"
        y1="12"
        x2="380"
        y2={height - 18}
        stroke="var(--border)"
        strokeDasharray="4 5"
      />
      {Array.from({ length: rows }).map((_, index) => {
        const y = 48 + index * 42
        return (
          <g key={index}>
            <rect
              x="24"
              y={y}
              width="292"
              height="32"
              rx="7"
              fill="var(--muted)"
              stroke="var(--border)"
            />
            <text
              x="170"
              y={y + 21}
              textAnchor="middle"
              fill="var(--muted-foreground)"
              fontSize="11"
            >
              {leftItems[index] ?? ""}
            </text>
            <line
              x1="316"
              y1={y + 16}
              x2="428"
              y2={y + 16}
              stroke="#0284c7"
              markerEnd={`url(#${id}-arrow)`}
            />
            <rect
              x="428"
              y={y}
              width="308"
              height="32"
              rx="7"
              fill={TONE.store.fill}
              stroke="#0284c7"
            />
            <text
              x="582"
              y={y + 21}
              textAnchor="middle"
              fill="var(--foreground)"
              fontSize="11"
            >
              {rightItems[index] ?? ""}
            </text>
          </g>
        )
      })}
    </svg>
  )
}

function LayerDiagram({
  id,
  label,
  layers,
}: {
  id: string
  label: string
  layers: readonly FlowItem[]
}) {
  const height = 34 + layers.length * 78
  return (
    <svg
      viewBox={`0 0 620 ${height}`}
      className="h-auto w-full min-w-[34rem]"
      role="img"
      aria-label={label}
    >
      <defs>
        <ArrowMarker id={`${id}-arrow`} />
      </defs>
      {layers.map((layer, index) => {
        const y = 16 + index * 78
        const colors = TONE[layer.tone]
        return (
          <g key={layer.label}>
            <rect
              x="90"
              y={y}
              width="440"
              height="54"
              rx="9"
              fill={colors.fill}
              stroke={colors.stroke}
              strokeWidth="1.5"
            />
            <text
              x="116"
              y={y + 24}
              fill="var(--foreground)"
              fontSize="13"
              fontWeight="650"
            >
              {layer.label}
            </text>
            <text
              x="116"
              y={y + 42}
              fill="var(--muted-foreground)"
              fontFamily="ui-monospace, SFMono-Regular, Menlo, monospace"
              fontSize="10"
            >
              {layer.detail}
            </text>
            {index < layers.length - 1 ? (
              <line
                x1="310"
                y1={y + 54}
                x2="310"
                y2={y + 78}
                stroke="#64748b"
                markerEnd={`url(#${id}-arrow)`}
              />
            ) : null}
          </g>
        )
      })}
    </svg>
  )
}

const mentalModel = [
  { label: "Working tree", detail: "normal files", tone: "muted" },
  { label: "Git + Crab", detail: "history + pointers", tone: "git" },
  { label: "Object storage", detail: "packs + xorbs", tone: "store" },
] as const

export function CrabMentalModelDiagram() {
  return (
    <DiagramFrame
      title="One repository, two representations"
      caption="Git keeps history and path identity. Crab stores large-file bytes in object storage and commits a pointer to the same history."
    >
      <FlowDiagram
        id="mental-model"
        label="Crab mental model"
        items={mentalModel}
        footer="No Crab data server sits in the byte path."
      />
    </DiagramFrame>
  )
}

export function LargeFileProblemDiagram() {
  return (
    <DiagramFrame
      title="The architectural decision"
      caption="Crab removes the large-file proxy while retaining Git as the repository interface."
    >
      <CompareDiagram
        id="large-file-problem"
        label="Server-mediated large-file storage compared with Crab"
        leftTitle="Server-mediated path"
        leftItems={[
          "Git client",
          "Large-file service",
          "Provider storage",
          "Service operations",
        ]}
        rightTitle="Crab path"
        rightItems={[
          "Git client + Crab",
          "Direct object-store transfer",
          "Your bucket",
          "Provider-managed durability",
        ]}
      />
    </DiagramFrame>
  )
}

export function LfsComparisonDiagram() {
  return (
    <DiagramFrame
      title="Where the large bytes travel"
      caption="Both systems keep pointers in Git. Their storage path and reuse unit differ."
    >
      <CompareDiagram
        id="lfs-comparison"
        label="Git LFS and Crab architecture comparison"
        leftTitle="Git LFS"
        leftItems={[
          "Git LFS pointer",
          "Batch API or transfer agent",
          "Whole-file object",
          "LFS service policy",
        ]}
        rightTitle="Crab"
        rightItems={[
          "Crab pointer",
          "Remote helper + filter",
          "Chunk-packed xorb",
          "Bucket and Git policy",
        ]}
      />
    </DiagramFrame>
  )
}

export function FirstRepositoryDiagram() {
  const items = [
    { label: "Configure", detail: "connect bucket", tone: "control" },
    { label: "Track", detail: "select paths", tone: "git" },
    { label: "Ship", detail: "add + commit + push", tone: "data" },
    { label: "Clone", detail: "fetch history", tone: "store" },
    { label: "Hydrate", detail: "verify bytes", tone: "safe" },
  ] as const
  return (
    <DiagramFrame
      title="The first-repository path"
      caption="The setup selects a bucket and tracking rules. Later commands preserve those choices."
    >
      <FlowDiagram
        id="first-repository"
        label="Five steps for a first Crab repository"
        items={items}
      />
    </DiagramFrame>
  )
}

export function FirstPushStateDiagram() {
  const layers = [
    {
      label: "Worktree",
      detail: "complete source and model bytes",
      tone: "muted",
    },
    { label: "Git index", detail: "source blob + Crab pointer", tone: "git" },
    {
      label: "Local staging",
      detail: "ordered chunks + file recipe",
      tone: "data",
    },
    {
      label: "Object storage",
      detail: "Git pack + xorbs + shards",
      tone: "store",
    },
    {
      label: "Visible ref",
      detail: "moves only after dependency proof",
      tone: "safe",
    },
  ] as const
  return (
    <DiagramFrame
      title="State after a successful push"
      caption="Each command changes one owned surface. The branch becomes visible only after every immutable dependency is durable."
    >
      <LayerDiagram
        id="first-push"
        label="Crab state from worktree to visible ref"
        layers={layers}
      />
    </DiagramFrame>
  )
}

export function DedupPipelineDiagram() {
  const items = [
    { label: "File bytes", detail: "bounded stream", tone: "muted" },
    { label: "Chunking", detail: "content-defined", tone: "data" },
    { label: "BLAKE3", detail: "chunk identity", tone: "control" },
    { label: "Dedup index", detail: "known or new", tone: "store" },
    { label: "Xorb", detail: "new chunks only", tone: "safe" },
  ] as const
  return (
    <DiagramFrame
      title="From file bytes to reusable storage"
      caption="Content-defined chunking preserves reuse across nearby edits. Only chunks that lack remote proof enter a new xorb."
    >
      <FlowDiagram
        id="dedup-pipeline"
        label="Crab deduplication pipeline"
        items={items}
      />
    </DiagramFrame>
  )
}

export function CacheHierarchyDiagram() {
  const layers = [
    {
      label: "Pointer and file recipe",
      detail: "identify ordered chunks",
      tone: "control",
    },
    {
      label: "Metadata indexes",
      detail: "locate chunk byte ranges",
      tone: "git",
    },
    {
      label: "Local staging",
      detail: "reuse unpublished or recent chunks",
      tone: "data",
    },
    {
      label: "Local verified cache",
      detail: "reuse immutable bytes",
      tone: "safe",
    },
    {
      label: "Optional shared cache",
      detail: "team-level immutable reuse",
      tone: "control",
    },
    {
      label: "Object-store origin",
      detail: "canonical remote read",
      tone: "store",
    },
  ] as const
  return (
    <DiagramFrame
      title="Resolve locally before reading the origin"
      caption="Metadata first resolves each chunk to a byte range. Crab then checks local and optional shared sources before a miss falls through to canonical object storage."
    >
      <LayerDiagram
        id="cache-hierarchy"
        label="Crab cache resolution hierarchy"
        layers={layers}
      />
    </DiagramFrame>
  )
}

export function HydrationPlanDiagram() {
  const items = [
    { label: "Pointer", detail: "hash + size", tone: "git" },
    { label: "Shard terms", detail: "every chunk", tone: "data" },
    { label: "Range plan", detail: "coalesced reads", tone: "control" },
    { label: "Rebuild", detail: "recipe order", tone: "store" },
    { label: "Verify", detail: "full-file hash", tone: "safe" },
  ] as const
  return (
    <DiagramFrame
      title="Hydration is a proof pipeline"
      caption="The reader resolves complete reconstruction terms, reduces request fan-out, and verifies the assembled file before replacement."
    >
      <FlowDiagram
        id="hydration-plan"
        label="Hydration resolution and verification pipeline"
        items={items}
      />
    </DiagramFrame>
  )
}

export function LazyCheckoutDiagram() {
  return (
    <DiagramFrame
      title="Choose when bytes become local"
      caption="All three paths share the same committed pointer identity. They differ only in when full content is materialized."
    >
      <CompareDiagram
        id="lazy-checkout"
        label="Pointer, hydration, and mount access choices"
        leftTitle="Need"
        leftItems={[
          "Git operations only",
          "Known files for a task",
          "Unpredictable read access",
        ]}
        rightTitle="Use"
        rightItems={["Keep pointers", "crab hydrate paths", "crab mount"]}
      />
    </DiagramFrame>
  )
}

export function GarbageCollectionDiagram() {
  const items = [
    { label: "Mark", detail: "all retained roots", tone: "git" },
    { label: "Classify", detail: "reachable or orphan", tone: "control" },
    { label: "Protect", detail: "grace window", tone: "data" },
    { label: "Sweep", detail: "proven orphans", tone: "store" },
    { label: "Report", detail: "explicit counters", tone: "safe" },
  ] as const
  return (
    <DiagramFrame
      title="Garbage collection fails toward retention"
      caption="An object must be unreachable from every retained root and older than the grace window before deletion."
    >
      <FlowDiagram
        id="garbage-collection"
        label="Crab garbage collection lifecycle"
        items={items}
      />
    </DiagramFrame>
  )
}

export function StorageTierDiagram() {
  const items = [
    { label: "Standard", detail: "new xorbs", tone: "store" },
    { label: "Warm class", detail: "configured age", tone: "control" },
    { label: "Archive class", detail: "restore may apply", tone: "data" },
    { label: "Hydration", detail: "class-aware read", tone: "safe" },
  ] as const
  return (
    <DiagramFrame
      title="Tier data, not repository metadata"
      caption="Lifecycle rules apply to immutable xorbs. Refs, manifests, shards, file indexes, and Git packs remain readable without an archive restore."
    >
      <FlowDiagram
        id="storage-tier"
        label="Crab object storage tier lifecycle"
        items={items}
      />
    </DiagramFrame>
  )
}

export function LfsMigrationDiagram() {
  const items = [
    { label: "Inspect", detail: "pointers + history", tone: "muted" },
    { label: "Choose path", detail: "transfer or rewrite", tone: "control" },
    { label: "Migrate", detail: "explicit scope", tone: "data" },
    { label: "Verify", detail: "status + fsck", tone: "safe" },
    { label: "Coordinate", detail: "shared refs", tone: "git" },
  ] as const
  return (
    <DiagramFrame
      title="Compatibility and migration are separate decisions"
      caption="A transfer integration preserves LFS pointers. A history migration changes Git object identities and requires coordinated verification."
    >
      <FlowDiagram
        id="lfs-migration"
        label="Git LFS compatibility and migration workflow"
        items={items}
      />
    </DiagramFrame>
  )
}
