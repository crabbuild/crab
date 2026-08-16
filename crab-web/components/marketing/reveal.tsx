"use client"

import { useEffect, useRef, useState } from "react"

interface RevealProps {
  children: React.ReactNode
  className?: string
  /** IntersectionObserver visibility threshold. Defaults to 0.05. */
  threshold?: number
  /** Transition duration in milliseconds. Defaults to 400. */
  duration?: number
}

/**
 * Scroll-reveal wrapper that fades and translates children into view once
 * they enter the viewport.
 *
 * One-shot behavior — once revealed, the animation does not re-trigger on
 * subsequent scroll events. Respects `prefers-reduced-motion` via the
 * `motion-safe:` Tailwind variant (content renders at full opacity
 * immediately when reduced motion is active). Falls back to full opacity
 * if IntersectionObserver is unavailable.
 */
export function Reveal({
  children,
  className = "",
  threshold = 0.05,
  duration = 400,
}: RevealProps) {
  const ref = useRef<HTMLDivElement>(null)
  const [visible, setVisible] = useState(false)

  useEffect(() => {
    const supportsIO = typeof IntersectionObserver !== "undefined"
    if (!supportsIO) {
      const frame = requestAnimationFrame(() => setVisible(true))
      return () => cancelAnimationFrame(frame)
    }

    const el = ref.current
    if (!el) return
    const io = new IntersectionObserver(
      ([entry]) => {
        if (entry.isIntersecting) {
          setVisible(true)
          io.unobserve(el)
        }
      },
      { threshold },
    )
    io.observe(el)
    return () => io.disconnect()
  }, [threshold])

  return (
    <div
      ref={ref}
      className={[
        "motion-safe:transition-all motion-safe:ease-out",
        visible
          ? "opacity-100 translate-y-0"
          : "motion-safe:opacity-0 motion-safe:translate-y-4",
        className,
      ]
        .filter(Boolean)
        .join(" ")}
      style={{ transitionDuration: `${duration}ms` }}
    >
      {children}
    </div>
  )
}
