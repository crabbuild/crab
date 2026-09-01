import type { CSSProperties, ReactNode } from "react"

export type LaunchMotionStory = "dedup" | "publish" | "hydrate" | "gc"

export type MotionSceneProps = {
  phase: number
  id: string
  animate: boolean
}

export const MONO = "ui-monospace, SFMono-Regular, Menlo, monospace"
export const SANS = "Inter, ui-sans-serif, system-ui"

export function transition(animate: boolean, delay = 0): CSSProperties {
  return {
    transition: animate
      ? `opacity 520ms ease ${delay}ms, transform 700ms cubic-bezier(.16,1,.3,1) ${delay}ms`
      : "none",
    transformBox: "fill-box",
    transformOrigin: "center",
  }
}

export function SceneDefs({ id, animate }: { id: string; animate: boolean }) {
  return (
    <defs>
      <linearGradient id={`${id}-panel`} x1="0" y1="0" x2="1" y2="1">
        <stop stopColor="#0f2031" />
        <stop offset="1" stopColor="#07101b" />
      </linearGradient>
      <linearGradient id={`${id}-cyan`} x1="0" y1="0" x2="1" y2="0">
        <stop stopColor="#22d3ee" stopOpacity="0.95" />
        <stop offset="1" stopColor="#38bdf8" stopOpacity="0.35" />
      </linearGradient>
      <linearGradient id={`${id}-amber`} x1="0" y1="0" x2="1" y2="0">
        <stop stopColor="#fde047" />
        <stop offset="1" stopColor="#fb923c" />
      </linearGradient>
      <filter id={`${id}-glow`} x="-100%" y="-100%" width="300%" height="300%">
        <feGaussianBlur stdDeviation="5" result="blur" />
        <feMerge>
          <feMergeNode in="blur" />
          <feMergeNode in="SourceGraphic" />
        </feMerge>
      </filter>
      <marker
        id={`${id}-arrow`}
        viewBox="0 0 10 10"
        refX="9"
        refY="5"
        markerWidth="8"
        markerHeight="8"
        orient="auto"
      >
        <path d="M1 1 9 5 1 9Z" fill="#64748b" />
      </marker>
      <style>{`
        @keyframes crab-launch-flow { to { stroke-dashoffset: -28; } }
        @keyframes crab-launch-orbit { 50% { transform: translateY(-5px); opacity: 1; } }
        .crab-launch-flow { animation: crab-launch-flow 900ms linear infinite; animation-play-state: ${animate ? "running" : "paused"}; }
        .crab-launch-orbit { animation: crab-launch-orbit 1.6s ease-in-out infinite; animation-play-state: ${animate ? "running" : "paused"}; }
      `}</style>
    </defs>
  )
}

export function StageLabel({
  children,
  x,
  y,
}: {
  children: ReactNode
  x: number
  y: number
}) {
  return (
    <text
      x={x}
      y={y}
      fill="#64748b"
      fontFamily={MONO}
      fontSize="10"
      fontWeight="700"
      letterSpacing="1.7"
    >
      {children}
    </text>
  )
}

function CheckMark({
  x,
  y,
  visible,
}: {
  x: number
  y: number
  visible: boolean
}) {
  return (
    <g
      style={{
        ...transition(true),
        opacity: visible ? 1 : 0.12,
        transform: visible ? "scale(1)" : "scale(.72)",
      }}
    >
      <circle cx={x} cy={y} r="11" fill="#052e2b" stroke="#34d399" />
      <path
        d={`M${x - 5} ${y}l3.5 3.5L${x + 6} ${y - 5}`}
        fill="none"
        stroke="#6ee7b7"
        strokeWidth="2.2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </g>
  )
}

function Database({
  x,
  y,
  color = "#38bdf8",
}: {
  x: number
  y: number
  color?: string
}) {
  return (
    <g fill="#0a1725" stroke={color} strokeWidth="1.4">
      <ellipse cx={x} cy={y} rx="54" ry="13" />
      <path d={`M${x - 54} ${y}v60c0 7 24 13 54 13s54-6 54-13v-60`} />
      <path
        d={`M${x - 54} ${y + 30}c0 7 24 13 54 13s54-6 54-13`}
        opacity="0.55"
      />
    </g>
  )
}

export function DedupMotionScene({ phase, id, animate }: MotionSceneProps) {
  const chunks = [
    { width: 72, tone: "reuse", label: "A7" },
    { width: 54, tone: "reuse", label: "B2" },
    { width: 82, tone: "reuse", label: "C9" },
    { width: 48, tone: "new", label: "N4" },
    { width: 68, tone: "new", label: "N5" },
    { width: 61, tone: "reuse", label: "D1" },
    { width: 76, tone: "reuse", label: "E8" },
  ] as const
  let cursor = 58

  return (
    <svg
      viewBox="0 0 900 420"
      className="block h-auto w-full min-w-[42rem]"
      role="img"
    >
      <title>Content-defined chunk reuse animation</title>
      <desc>
        A new checkpoint is split into stable content-defined chunks. Existing
        cyan chunks are reused while only two amber chunks enter a new xorb.
      </desc>
      <SceneDefs id={id} animate={animate} />
      <rect width="900" height="420" fill={`url(#${id}-panel)`} />
      <path d="M0 58H900M0 335H900" stroke="#1e293b" />
      <StageLabel x={36} y={35}>
        NEW CHECKPOINT · 8 GB LOGICAL
      </StageLabel>
      <StageLabel x={36} y={368}>
        IDENTITY, NOT FILE NAMES, DECIDES REUSE
      </StageLabel>

      <rect
        x="36"
        y="76"
        width="602"
        height="105"
        rx="18"
        fill="#081522"
        stroke="#334155"
      />
      <text
        x="58"
        y="105"
        fill="#e2e8f0"
        fontFamily={SANS}
        fontSize="14"
        fontWeight="700"
      >
        encoder-v4.safetensors
      </text>
      <text
        x="611"
        y="105"
        textAnchor="end"
        fill="#64748b"
        fontFamily={MONO}
        fontSize="10"
      >
        STREAMING CDC
      </text>
      {chunks.map((chunk, index) => {
        const x = cursor
        cursor += chunk.width + 6
        const reused = chunk.tone === "reuse"
        const ready = phase >= 1
        return (
          <g
            key={chunk.label}
            style={{
              ...transition(animate, index * 45),
              opacity: ready ? 1 : 0.35,
              transform: ready ? "translateY(0)" : "translateY(-14px)",
            }}
          >
            <rect
              x={x}
              y="124"
              width={chunk.width}
              height="34"
              rx="7"
              fill={phase >= 2 ? (reused ? "#073344" : "#3b2810") : "#111f2d"}
              stroke={phase >= 2 ? (reused ? "#22d3ee" : "#fbbf24") : "#475569"}
            />
            <text
              x={x + chunk.width / 2}
              y="145"
              textAnchor="middle"
              fill={phase >= 2 ? (reused ? "#67e8f9" : "#fde68a") : "#94a3b8"}
              fontFamily={MONO}
              fontSize="10"
            >
              {chunk.label}
            </text>
          </g>
        )
      })}

      <g style={{ ...transition(animate), opacity: phase >= 2 ? 1 : 0 }}>
        {[94, 164, 240, 474, 544].map((x, index) => (
          <path
            key={x}
            d={`M${x} 160 C${x} 215 ${115 + index * 78} 218 ${115 + index * 78} 270`}
            fill="none"
            stroke="#22d3ee"
            strokeOpacity="0.42"
            strokeDasharray="5 7"
            className="crab-launch-flow"
          />
        ))}
      </g>

      <rect
        x="36"
        y="246"
        width="454"
        height="70"
        rx="15"
        fill="#071a25"
        stroke={phase >= 2 ? "#22d3ee" : "#334155"}
      />
      <text
        x="58"
        y="273"
        fill="#67e8f9"
        fontFamily={MONO}
        fontSize="10"
        letterSpacing="1.2"
      >
        REUSABLE BASE
      </text>
      <text
        x="58"
        y="298"
        fill="#cbd5e1"
        fontFamily={SANS}
        fontSize="15"
        fontWeight="700"
      >
        7.5 GB already durable
      </text>
      <g
        style={{
          ...transition(animate),
          opacity: phase >= 4 ? 1 : 0,
          transform: phase >= 4 ? "scale(1)" : "scale(.85)",
        }}
      >
        <rect
          x="304"
          y="263"
          width="160"
          height="35"
          rx="17.5"
          fill="#052e2b"
          stroke="#34d399"
        />
        <text
          x="384"
          y="285"
          textAnchor="middle"
          fill="#6ee7b7"
          fontFamily={MONO}
          fontSize="11"
          fontWeight="700"
        >
          94% REUSED
        </text>
      </g>

      <g
        style={{
          ...transition(animate),
          opacity: phase >= 3 ? 1 : 0,
          transform: phase >= 3 ? "translateX(0)" : "translateX(-36px)",
        }}
      >
        <path
          d="M637 142 C702 142 685 257 728 257"
          fill="none"
          stroke="#fbbf24"
          strokeWidth="2"
          strokeDasharray="7 7"
          className="crab-launch-flow"
          markerEnd={`url(#${id}-arrow)`}
        />
        <circle
          cx="678"
          cy="183"
          r="5"
          fill="#fde047"
          filter={`url(#${id}-glow)`}
          className="crab-launch-orbit"
        />
      </g>
      <Database x={786} y={239} color={phase >= 3 ? "#fbbf24" : "#475569"} />
      <text
        x="786"
        y="279"
        textAnchor="middle"
        fill="#e2e8f0"
        fontFamily={SANS}
        fontSize="13"
        fontWeight="700"
      >
        new xorb
      </text>
      <text
        x="786"
        y="297"
        textAnchor="middle"
        fill={phase >= 3 ? "#fde68a" : "#64748b"}
        fontFamily={MONO}
        fontSize="10"
      >
        0.5 GB uploaded
      </text>
      <CheckMark x={842} y={314} visible={phase >= 4} />
    </svg>
  )
}

export function PublishMotionScene({ phase, id, animate }: MotionSceneProps) {
  const immutableReady = phase >= 2
  const refVisible = phase >= 4

  return (
    <svg
      viewBox="0 0 900 420"
      className="block h-auto w-full min-w-[42rem]"
      role="img"
    >
      <title>Durable before visible publication animation</title>
      <desc>
        Git objects and Crab data upload through separate lanes. The publication
        gate opens only after both closures are durable, then the main ref
        advances.
      </desc>
      <SceneDefs id={id} animate={animate} />
      <rect width="900" height="420" fill={`url(#${id}-panel)`} />
      <StageLabel x={34} y={35}>
        PUSH · TWO IMMUTABLE LANES
      </StageLabel>
      <StageLabel x={656} y={35}>
        ONE VISIBILITY GATE
      </StageLabel>

      {[
        {
          y: 92,
          color: "#fb923c",
          title: "Git pack",
          detail: "commit · tree · pointer",
        },
        {
          y: 252,
          color: "#22d3ee",
          title: "Crab closure",
          detail: "recipe · shards · xorbs",
        },
      ].map((lane, laneIndex) => (
        <g key={lane.title}>
          <rect
            x="34"
            y={lane.y}
            width="186"
            height="82"
            rx="15"
            fill="#0a1725"
            stroke={lane.color}
            strokeOpacity="0.72"
          />
          <circle cx="65" cy={lane.y + 30} r="8" fill={lane.color} />
          <text
            x="84"
            y={lane.y + 34}
            fill="#e2e8f0"
            fontFamily={SANS}
            fontSize="14"
            fontWeight="700"
          >
            {lane.title}
          </text>
          <text
            x="65"
            y={lane.y + 59}
            fill="#64748b"
            fontFamily={MONO}
            fontSize="10"
          >
            {lane.detail}
          </text>
          <path
            d={`M220 ${lane.y + 41} H475`}
            fill="none"
            stroke={lane.color}
            strokeOpacity={phase >= laneIndex + 1 ? 0.9 : 0.2}
            strokeWidth="2"
            strokeDasharray="9 9"
            className={phase >= laneIndex + 1 ? "crab-launch-flow" : undefined}
            markerEnd={`url(#${id}-arrow)`}
          />
          {[0, 1, 2].map((packet) => (
            <rect
              key={packet}
              x={274 + packet * 63}
              y={lane.y + 32}
              width="20"
              height="18"
              rx="4"
              fill={lane.color}
              opacity={phase >= laneIndex + 1 ? 0.85 : 0.1}
              style={{
                ...transition(animate, packet * 90),
                transform:
                  phase >= laneIndex + 1
                    ? "translateX(0)"
                    : "translateX(-42px)",
              }}
            />
          ))}
        </g>
      ))}

      <rect
        x="490"
        y="76"
        width="160"
        height="282"
        rx="22"
        fill="#071a25"
        stroke={immutableReady ? "#34d399" : "#334155"}
      />
      <Database
        x={570}
        y={131}
        color={immutableReady ? "#38bdf8" : "#475569"}
      />
      <text
        x="570"
        y="212"
        textAnchor="middle"
        fill="#e2e8f0"
        fontFamily={SANS}
        fontSize="14"
        fontWeight="700"
      >
        your bucket
      </text>
      <text
        x="570"
        y="233"
        textAnchor="middle"
        fill="#64748b"
        fontFamily={MONO}
        fontSize="10"
      >
        canonical objects
      </text>
      <CheckMark x={535} y={289} visible={phase >= 1} />
      <text
        x="557"
        y="293"
        fill={phase >= 1 ? "#6ee7b7" : "#64748b"}
        fontFamily={MONO}
        fontSize="10"
      >
        Git closure
      </text>
      <CheckMark x={535} y={325} visible={phase >= 2} />
      <text
        x="557"
        y="329"
        fill={phase >= 2 ? "#6ee7b7" : "#64748b"}
        fontFamily={MONO}
        fontSize="10"
      >
        data closure
      </text>

      <g
        style={{
          ...transition(animate),
          transform: immutableReady ? "translateY(0)" : "translateY(-6px)",
        }}
      >
        <rect
          x="680"
          y="112"
          width="72"
          height="164"
          rx="16"
          fill={immutableReady ? "#052e2b" : "#28131a"}
          stroke={immutableReady ? "#34d399" : "#fb7185"}
          strokeWidth="2"
        />
        <path
          d="M716 150v57"
          stroke={immutableReady ? "#6ee7b7" : "#fda4af"}
          strokeWidth="5"
          strokeLinecap="round"
        />
        <circle
          cx="716"
          cy={immutableReady ? 226 : 191}
          r="12"
          fill={immutableReady ? "#34d399" : "#fb7185"}
          style={{
            ...transition(animate),
            transform: immutableReady ? "translateY(0)" : "translateY(-35px)",
          }}
        />
        <text
          x="716"
          y="258"
          textAnchor="middle"
          fill={immutableReady ? "#6ee7b7" : "#fda4af"}
          fontFamily={MONO}
          fontSize="9"
          fontWeight="700"
        >
          {immutableReady ? "OPEN" : "BLOCKED"}
        </text>
      </g>

      <path
        d="M650 194H680M752 194H854"
        stroke={refVisible ? "#34d399" : "#475569"}
        strokeWidth="2"
        markerEnd={`url(#${id}-arrow)`}
      />
      <circle
        cx={refVisible ? 829 : 783}
        cy="194"
        r="9"
        fill={refVisible ? "#34d399" : "#64748b"}
        filter={refVisible ? `url(#${id}-glow)` : undefined}
        style={transition(animate)}
      />
      <text
        x="804"
        y="157"
        textAnchor="middle"
        fill="#e2e8f0"
        fontFamily={SANS}
        fontSize="14"
        fontWeight="700"
      >
        main
      </text>
      <text
        x="804"
        y="226"
        textAnchor="middle"
        fill={refVisible ? "#6ee7b7" : "#94a3b8"}
        fontFamily={MONO}
        fontSize="10"
      >
        {refVisible ? "commit 8fc2 visible" : "old tip remains"}
      </text>
      <g style={{ ...transition(animate), opacity: phase >= 3 ? 1 : 0 }}>
        <rect
          x="674"
          y="318"
          width="196"
          height="44"
          rx="22"
          fill="#052e2b"
          stroke="#34d399"
        />
        <text
          x="772"
          y="345"
          textAnchor="middle"
          fill="#6ee7b7"
          fontFamily={MONO}
          fontSize="10"
          fontWeight="700"
        >
          EXPECTED-OLD MATCHES
        </text>
      </g>
    </svg>
  )
}
