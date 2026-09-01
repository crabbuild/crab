import {
  MONO,
  SANS,
  SceneDefs,
  StageLabel,
  transition,
  type MotionSceneProps,
} from "@/components/blog/launch-motion-scenes"

export function HydrateMotionScene({ phase, id, animate }: MotionSceneProps) {
  const selected = new Set([1, 3, 6, 8])

  return (
    <svg
      viewBox="0 0 900 420"
      className="block h-auto w-full min-w-[42rem]"
      role="img"
    >
      <title>Selective lazy hydration animation</title>
      <desc>
        A compact pointer resolves four selected xorb ranges. Only those ranges
        travel into a reconstructed file, which is verified before it appears in
        the worktree.
      </desc>
      <SceneDefs id={id} animate={animate} />
      <rect width="900" height="420" fill={`url(#${id}-panel)`} />
      <StageLabel x={35} y={34}>
        LAZY CHECKOUT · RANGE READS
      </StageLabel>
      <StageLabel x={644} y={34}>
        VERIFY, THEN MATERIALIZE
      </StageLabel>

      <rect
        x="34"
        y="78"
        width="164"
        height="250"
        rx="18"
        fill="#0a1725"
        stroke="#a78bfa"
      />
      <text
        x="58"
        y="111"
        fill="#c4b5fd"
        fontFamily={MONO}
        fontSize="10"
        letterSpacing="1.2"
      >
        GIT POINTER
      </text>
      <path
        d="M65 137h101v84H65zM80 157h71M80 175h52M80 193h64"
        fill="none"
        stroke="#64748b"
      />
      <text
        x="116"
        y="251"
        textAnchor="middle"
        fill="#e2e8f0"
        fontFamily={SANS}
        fontSize="13"
        fontWeight="700"
      >
        encoder.bin
      </text>
      <text
        x="116"
        y="272"
        textAnchor="middle"
        fill="#94a3b8"
        fontFamily={MONO}
        fontSize="10"
      >
        f41a… · 40 GB
      </text>
      <g
        style={{
          ...transition(animate),
          opacity: phase >= 1 ? 1 : 0,
          transform: phase >= 1 ? "translateY(0)" : "translateY(-10px)",
        }}
      >
        <rect
          x="59"
          y="292"
          width="114"
          height="26"
          rx="13"
          fill="#1f1835"
          stroke="#a78bfa"
        />
        <text
          x="116"
          y="309"
          textAnchor="middle"
          fill="#c4b5fd"
          fontFamily={MONO}
          fontSize="9"
        >
          RECIPE RESOLVED
        </text>
      </g>

      <path
        d="M198 202H272"
        stroke="#64748b"
        strokeWidth="1.5"
        markerEnd={`url(#${id}-arrow)`}
      />
      <rect
        x="278"
        y="64"
        width="292"
        height="292"
        rx="20"
        fill="#071a25"
        stroke={phase >= 1 ? "#38bdf8" : "#334155"}
      />
      <text
        x="302"
        y="98"
        fill="#7dd3fc"
        fontFamily={MONO}
        fontSize="10"
        letterSpacing="1.2"
      >
        XORB RANGE MAP
      </text>
      {Array.from({ length: 10 }, (_, index) => {
        const column = index % 5
        const row = Math.floor(index / 5)
        const x = 304 + column * 49
        const y = 131 + row * 68
        const isSelected = selected.has(index)
        return (
          <g
            key={index}
            style={{
              ...transition(animate, index * 35),
              opacity: phase >= 1 ? 1 : 0.35,
            }}
          >
            <rect
              x={x}
              y={y}
              width="36"
              height="46"
              rx="7"
              fill={isSelected && phase >= 2 ? "#073344" : "#101f2e"}
              stroke={isSelected && phase >= 2 ? "#22d3ee" : "#475569"}
            />
            <text
              x={x + 18}
              y={y + 27}
              textAnchor="middle"
              fill={isSelected && phase >= 2 ? "#67e8f9" : "#64748b"}
              fontFamily={MONO}
              fontSize="9"
            >
              {index + 1}
            </text>
          </g>
        )
      })}
      <text x="302" y="297" fill="#64748b" fontFamily={MONO} fontSize="9">
        4 ranges selected · 6 stay cold
      </text>
      <path d="M304 320H540" stroke="#1e293b" />
      <text
        x="422"
        y="340"
        textAnchor="middle"
        fill={phase >= 2 ? "#67e8f9" : "#64748b"}
        fontFamily={MONO}
        fontSize="10"
      >
        COALESCED RANGE GET
      </text>

      <path
        d="M570 204 C620 204 610 192 654 192"
        fill="none"
        stroke="#22d3ee"
        strokeOpacity={phase >= 2 ? 0.9 : 0.18}
        strokeWidth="2"
        strokeDasharray="8 8"
        className={phase >= 2 ? "crab-launch-flow" : undefined}
        markerEnd={`url(#${id}-arrow)`}
      />
      {[0, 1, 2, 3].map((index) => (
        <rect
          key={index}
          x={590 + index * 18}
          y={184}
          width="12"
          height="16"
          rx="3"
          fill="#22d3ee"
          opacity={phase >= 2 ? 0.85 : 0}
          style={{
            ...transition(animate, index * 100),
            transform: phase >= 2 ? "translateX(0)" : "translateX(-32px)",
          }}
        />
      ))}

      <g
        style={{
          ...transition(animate),
          opacity: phase >= 3 ? 1 : 0.32,
          transform: phase >= 3 ? "translateY(0)" : "translateY(10px)",
        }}
      >
        <path
          d="M680 88h139l34 34v190H680zM819 88v34h34"
          fill="#0a1725"
          stroke={phase >= 4 ? "#34d399" : "#38bdf8"}
          strokeWidth="2"
        />
        {[0, 1, 2, 3].map((index) => (
          <rect
            key={index}
            x={704}
            y={148 + index * 31}
            width={84 + index * 8}
            height="18"
            rx="4"
            fill={index % 2 ? "#0e7490" : "#155e75"}
            opacity="0.9"
          />
        ))}
        <text
          x="766"
          y="294"
          textAnchor="middle"
          fill="#e2e8f0"
          fontFamily={SANS}
          fontSize="13"
          fontWeight="700"
        >
          working file
        </text>
      </g>
      <g
        style={{
          ...transition(animate),
          opacity: phase >= 4 ? 1 : 0,
          transform: phase >= 4 ? "scale(1)" : "scale(.72)",
        }}
      >
        <circle
          cx="820"
          cy="318"
          r="33"
          fill="#052e2b"
          stroke="#34d399"
          strokeWidth="2"
          filter={`url(#${id}-glow)`}
        />
        <path
          d="M805 318l10 10 20-23"
          fill="none"
          stroke="#6ee7b7"
          strokeWidth="4"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
        <text
          x="820"
          y="374"
          textAnchor="middle"
          fill="#6ee7b7"
          fontFamily={MONO}
          fontSize="10"
          fontWeight="700"
        >
          BLAKE3 VERIFIED
        </text>
      </g>
    </svg>
  )
}

export function GcMotionScene({ phase, id, animate }: MotionSceneProps) {
  const nodes = [
    { x: 288, y: 104, label: "commit", live: true },
    { x: 400, y: 104, label: "recipe", live: true },
    { x: 512, y: 104, label: "xorb A", live: true },
    { x: 400, y: 226, label: "xorb B", live: false, young: true },
    { x: 512, y: 284, label: "xorb C", live: false, young: false },
  ] as const

  return (
    <svg
      viewBox="0 0 900 420"
      className="block h-auto w-full min-w-[42rem]"
      role="img"
    >
      <title>Reachability-safe garbage collection animation</title>
      <desc>
        Retained roots mark a commit, recipe, and xorb as live. A recent orphan
        remains protected by the grace window while only an old unreachable xorb
        becomes eligible for deletion.
      </desc>
      <SceneDefs id={id} animate={animate} />
      <rect width="900" height="420" fill={`url(#${id}-panel)`} />
      <StageLabel x={35} y={35}>
        MARK · AGE · SWEEP
      </StageLabel>
      <StageLabel x={672} y={35}>
        REPOSITORY SCOPE ONLY
      </StageLabel>

      <rect
        x="34"
        y="78"
        width="172"
        height="250"
        rx="18"
        fill="#0a1725"
        stroke="#a78bfa"
      />
      <text
        x="58"
        y="112"
        fill="#c4b5fd"
        fontFamily={MONO}
        fontSize="10"
        letterSpacing="1.2"
      >
        RETAINED ROOTS
      </text>
      {[
        [58, 149, "main"],
        [58, 201, "release/v1"],
        [58, 253, "recovery"],
      ].map(([x, y, label], index) => (
        <g
          key={String(label)}
          style={{
            ...transition(animate, index * 90),
            opacity: phase >= 1 ? 1 : 0.35,
            transform: phase >= 1 ? "translateX(0)" : "translateX(-14px)",
          }}
        >
          <circle cx={Number(x) + 9} cy={Number(y) - 4} r="8" fill="#8b5cf6" />
          <text
            x={Number(x) + 28}
            y={Number(y)}
            fill="#cbd5e1"
            fontFamily={MONO}
            fontSize="10"
          >
            {label}
          </text>
        </g>
      ))}

      <path
        d="M206 185 C244 185 240 104 266 104"
        fill="none"
        stroke={phase >= 1 ? "#a78bfa" : "#334155"}
        strokeWidth="2"
        strokeDasharray="7 7"
        className={phase >= 1 ? "crab-launch-flow" : undefined}
        markerEnd={`url(#${id}-arrow)`}
      />
      <path
        d="M310 104H378M422 104H490"
        fill="none"
        stroke={phase >= 2 ? "#34d399" : "#334155"}
        strokeWidth="2"
        markerEnd={`url(#${id}-arrow)`}
      />
      <path
        d="M400 126V204M434 119C468 151 485 232 503 262"
        fill="none"
        stroke="#475569"
        strokeDasharray="5 7"
      />

      {nodes.map((node, index) => {
        const marked = node.live && phase >= 2
        const oldEligible = !node.live && !node.young && phase >= 4
        const youngProtected = !node.live && node.young && phase >= 3
        const color = marked
          ? "#34d399"
          : youngProtected
            ? "#fbbf24"
            : oldEligible
              ? "#fb7185"
              : "#64748b"
        return (
          <g
            key={node.label}
            style={{
              ...transition(animate, index * 45),
              opacity: oldEligible ? 0.18 : 1,
              transform: oldEligible
                ? "scale(.72)"
                : marked
                  ? "scale(1.05)"
                  : "scale(1)",
            }}
          >
            <circle
              cx={node.x}
              cy={node.y}
              r="22"
              fill={marked ? "#052e2b" : youngProtected ? "#31240b" : "#111f2d"}
              stroke={color}
              strokeWidth={marked || youngProtected ? 2 : 1.4}
              filter={marked ? `url(#${id}-glow)` : undefined}
            />
            <text
              x={node.x}
              y={node.y + 4}
              textAnchor="middle"
              fill={color}
              fontFamily={MONO}
              fontSize="8"
              fontWeight="700"
            >
              {node.label}
            </text>
          </g>
        )
      })}

      <g
        style={{
          ...transition(animate),
          opacity: phase >= 3 ? 1 : 0,
          transform: phase >= 3 ? "translateY(0)" : "translateY(12px)",
        }}
      >
        <circle cx="400" cy="280" r="17" fill="#31240b" stroke="#fbbf24" />
        <path
          d="M400 270v11l8 5"
          fill="none"
          stroke="#fde68a"
          strokeWidth="2"
          strokeLinecap="round"
        />
        <text
          x="400"
          y="317"
          textAnchor="middle"
          fill="#fde68a"
          fontFamily={MONO}
          fontSize="9"
        >
          GRACE WINDOW
        </text>
      </g>

      <path
        d="M560 104 C626 104 611 121 654 121M538 284 C616 284 600 287 654 287"
        fill="none"
        stroke="#475569"
        markerEnd={`url(#${id}-arrow)`}
      />
      <rect
        x="666"
        y="78"
        width="196"
        height="86"
        rx="16"
        fill="#071d1b"
        stroke="#34d399"
      />
      <text
        x="690"
        y="109"
        fill="#6ee7b7"
        fontFamily={MONO}
        fontSize="10"
        letterSpacing="1.1"
      >
        RETAIN
      </text>
      <text
        x="690"
        y="137"
        fill="#e2e8f0"
        fontFamily={SANS}
        fontSize="14"
        fontWeight="700"
      >
        reachable closure
      </text>
      <rect
        x="666"
        y="190"
        width="196"
        height="72"
        rx="16"
        fill="#251f0b"
        stroke="#fbbf24"
      />
      <text
        x="690"
        y="219"
        fill="#fde68a"
        fontFamily={MONO}
        fontSize="10"
        letterSpacing="1.1"
      >
        PROTECT
      </text>
      <text
        x="690"
        y="244"
        fill="#e2e8f0"
        fontFamily={SANS}
        fontSize="13"
        fontWeight="700"
      >
        recent orphan
      </text>
      <g style={{ ...transition(animate), opacity: phase >= 4 ? 1 : 0.35 }}>
        <rect
          x="666"
          y="288"
          width="196"
          height="72"
          rx="16"
          fill="#2a111b"
          stroke="#fb7185"
        />
        <text
          x="690"
          y="317"
          fill="#fda4af"
          fontFamily={MONO}
          fontSize="10"
          letterSpacing="1.1"
        >
          ELIGIBLE
        </text>
        <text
          x="690"
          y="342"
          fill="#e2e8f0"
          fontFamily={SANS}
          fontSize="13"
          fontWeight="700"
        >
          old + unreachable
        </text>
      </g>
      <text x="35" y="388" fill="#64748b" fontFamily={MONO} fontSize="10">
        Deleting a path is not proof. Reachability is.
      </text>
    </svg>
  )
}
