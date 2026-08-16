"use client"

import { useEffect, useMemo, useRef, useState } from "react"

import { cn } from "@/lib/utils"

/** A single line in the terminal demo. */
export interface TypingLine {
  text: string
  /**
   * Visual style for the line:
   * - `command` — user-typed command (brighter, optionally prefixed with `$ `).
   * - `output`  — command output (muted, current default).
   * - `comment` — inline commentary (italic, muted further).
   *
   * If unset, the line type is inferred from the text: lines starting with
   * `$ ` are treated as commands, lines starting with `#` as comments,
   * and everything else as output. This preserves the visual fidelity of
   * existing call sites that pass plain strings.
   */
  type?: "command" | "output" | "comment"
}

/** Input shape for `lines` — accepts either plain strings or structured lines. */
export type TypingLineInput = string | TypingLine

interface TypingCodeProps {
  /**
   * Lines to type out sequentially. Plain strings are accepted for backward
   * compatibility — their type is inferred from the leading characters.
   */
  lines: TypingLineInput[]
  /** Minimum per-character delay in milliseconds. Defaults to 25. */
  charDelay?: number
  /** Random jitter added to charDelay (0–jitter ms). Defaults to 15. */
  charJitter?: number
  /**
   * Pause in milliseconds inserted after a line finishes typing, before the
   * next line begins. Defaults to 500ms (within the 300–800ms range from
   * Requirement 4.2).
   */
  lineDelay?: number
  /** IntersectionObserver visibility threshold. Defaults to 0.5 (50% in view). */
  threshold?: number
  /** Optional title shown centered in the terminal title bar (e.g. "Crab CLI"). */
  title?: string
  className?: string
}

/**
 * Terminal-style typing animation that types out lines one character at a
 * time. The animation triggers once the element scrolls into view (one-shot)
 * and is disabled (rendered in final state immediately) when the user has
 * `prefers-reduced-motion: reduce` set.
 */
export function TypingCode({
  lines,
  charDelay = 25,
  charJitter = 15,
  lineDelay = 500,
  threshold = 0.5,
  title,
  className,
}: TypingCodeProps) {
  // Normalize the input to structured lines once per `lines` change.
  const normalizedLines = useMemo<TypingLine[]>(
    () => lines.map(normalizeLine),
    [lines],
  )

  const containerRef = useRef<HTMLDivElement>(null)
  const [active, setActive] = useState(false)
  const [reducedMotion, setReducedMotion] = useState(false)
  const [doneCount, setDoneCount] = useState(0)
  const [cur, setCur] = useState("")
  const [charIdx, setCharIdx] = useState(0)
  const [pausing, setPausing] = useState(false)

  // Detect prefers-reduced-motion. When set, render all lines immediately at
  // their final state and skip the animation entirely.
  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return
    const mq = window.matchMedia("(prefers-reduced-motion: reduce)")
    setReducedMotion(mq.matches)
    const handler = (e: MediaQueryListEvent) => setReducedMotion(e.matches)
    mq.addEventListener?.("change", handler)
    return () => mq.removeEventListener?.("change", handler)
  }, [])

  // Trigger animation when the container scrolls into view. Falls back to
  // showing all lines immediately if IntersectionObserver isn't available.
  useEffect(() => {
    const el = containerRef.current
    if (!el) return
    if (typeof IntersectionObserver === "undefined") {
      setActive(true)
      return
    }
    const io = new IntersectionObserver(
      ([entry]) => {
        if (entry.isIntersecting) {
          setActive(true)
          io.unobserve(el)
        }
      },
      { threshold },
    )
    io.observe(el)
    return () => io.disconnect()
  }, [threshold])

  // Character-by-character typing effect, with an inter-line pause.
  useEffect(() => {
    if (!active || reducedMotion) return
    if (doneCount >= normalizedLines.length) return

    const line = normalizedLines[doneCount].text

    if (pausing) {
      const t = setTimeout(() => {
        setPausing(false)
        setCur("")
        setCharIdx(0)
        setDoneCount((n) => n + 1)
      }, Math.max(0, lineDelay))
      return () => clearTimeout(t)
    }

    if (charIdx <= line.length) {
      const delay = charDelay + Math.random() * charJitter
      const t = setTimeout(() => {
        setCur(line.slice(0, charIdx))
        setCharIdx((c) => c + 1)
      }, delay)
      return () => clearTimeout(t)
    }

    // Finished typing this line — enter inter-line pause before advancing.
    setPausing(true)
  }, [
    active,
    reducedMotion,
    doneCount,
    charIdx,
    pausing,
    normalizedLines,
    charDelay,
    charJitter,
    lineDelay,
  ])

  // When reduced motion is preferred, render every line in its final state
  // and skip the cursor entirely.
  const renderReduced = reducedMotion
  const completedCount = renderReduced ? normalizedLines.length : doneCount
  const showCurrentLine =
    !renderReduced && active && doneCount < normalizedLines.length

  return (
    <div ref={containerRef} className={className}>
      <div className="overflow-hidden rounded-card bg-[#1a1a2e]">
        <TerminalTitleBar title={title} />
        <pre className="m-0 min-h-[100px] overflow-x-auto px-[18px] py-[14px] font-mono text-[12.5px] leading-[1.7] text-[#9ca3af] text-left">
          {normalizedLines.slice(0, completedCount).map((line, i) => (
            <LineRow key={i} line={line} />
          ))}
          {showCurrentLine && (
            <LineRow
              line={normalizedLines[doneCount]}
              partialText={cur}
              showCursor
            />
          )}
        </pre>
      </div>
    </div>
  )
}

function TerminalTitleBar({ title }: { title?: string }) {
  return (
    <div
      className="relative flex items-center gap-[7px] bg-[#14142a] px-[14px] py-[10px]"
      aria-hidden="true"
    >
      <span className="h-[10px] w-[10px] rounded-full bg-[#ff5f57]" />
      <span className="h-[10px] w-[10px] rounded-full bg-[#febc2e]" />
      <span className="h-[10px] w-[10px] rounded-full bg-[#28c840]" />
      {title && (
        <span className="pointer-events-none absolute inset-x-0 text-center font-mono text-[11px] tracking-wide text-[#6b7280]">
          {title}
        </span>
      )}
    </div>
  )
}

function LineRow({
  line,
  partialText,
  showCursor,
}: {
  line: TypingLine
  partialText?: string
  showCursor?: boolean
}) {
  const text = partialText ?? line.text
  const type = line.type ?? "output"

  // Empty lines render as a blank row regardless of type so spacing is
  // preserved without any prefix/styling artifacts.
  if (!text && !showCursor) {
    return <div>&nbsp;</div>
  }

  const className = cn(
    type === "command" && "text-[#e5e7eb]",
    type === "output" && "text-[#9ca3af]",
    type === "comment" && "italic text-[#6b7280]",
  )

  // Commands get a green `$` prefix when the underlying text doesn't already
  // start with one. The prompt is rendered as a sibling so the typing effect
  // operates on the user-supplied text only.
  const showPrompt = type === "command" && !line.text.startsWith("$")

  return (
    <div className={className}>
      {showPrompt && <span className="text-[#28c840]">$ </span>}
      {text}
      {showCursor && (
        <span className="animate-[typing-cursor-blink_1s_step-end_infinite] text-primary">
          ▊
        </span>
      )}
    </div>
  )
}

/**
 * Infer a line's type from its leading characters when one isn't provided.
 * Preserves visual fidelity for callers that pass plain strings (the prior
 * API) where commands begin with `$ ` and comments with `#`.
 */
function normalizeLine(input: TypingLineInput): TypingLine {
  if (typeof input !== "string") {
    return { text: input.text, type: input.type }
  }
  const trimmed = input.trimStart()
  if (trimmed.startsWith("$ ") || trimmed === "$") {
    return { text: input, type: "command" }
  }
  if (trimmed.startsWith("#")) {
    return { text: input, type: "comment" }
  }
  return { text: input, type: "output" }
}
