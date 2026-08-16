"use client"

import { useEffect, useRef, useState } from "react"

interface CounterProps {
  /** Target value to count up to. */
  end: number
  /** Optional suffix appended after the numeric value (e.g. "+ MB/s", "%", "×"). */
  suffix?: string
  /** Animation duration in milliseconds. Defaults to 1600. */
  duration?: number
  /** IntersectionObserver visibility threshold. Defaults to 0.5. */
  threshold?: number
  className?: string
}

/**
 * Animated counter that ramps from 0 to `end` once the element scrolls into view.
 *
 * The animation is one-shot — once triggered, the observer disconnects and the
 * value remains at `end`. Falls back to displaying `end` immediately if
 * IntersectionObserver is unavailable. Respects `prefers-reduced-motion` by
 * showing the final value immediately without animation.
 */
export function Counter({
  end,
  suffix = "",
  duration = 1600,
  threshold = 0.5,
  className,
}: CounterProps) {
  const ref = useRef<HTMLSpanElement>(null)
  const [started, setStarted] = useState(false)
  const [reducedMotion, setReducedMotion] = useState(false)
  const [val, setVal] = useState(0)

  // Detect prefers-reduced-motion. When set, skip the count-up animation
  // and display the final value immediately.
  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return
    const mq = window.matchMedia("(prefers-reduced-motion: reduce)")
    setReducedMotion(mq.matches)
    const handler = (e: MediaQueryListEvent) => setReducedMotion(e.matches)
    mq.addEventListener("change", handler)
    return () => mq.removeEventListener("change", handler)
  }, [])

  useEffect(() => {
    const el = ref.current
    if (!el) return
    if (typeof IntersectionObserver === "undefined") {
      setStarted(true)
      return
    }
    const io = new IntersectionObserver(
      ([e]) => {
        if (e.isIntersecting) {
          setStarted(true)
          io.unobserve(el)
        }
      },
      { threshold },
    )
    io.observe(el)
    return () => io.disconnect()
  }, [threshold])

  useEffect(() => {
    if (!started) return
    // When reduced motion is preferred, jump straight to the end value.
    if (reducedMotion) {
      setVal(end)
      return
    }
    const t0 = performance.now()
    let raf = 0
    const tick = (now: number) => {
      const p = Math.min((now - t0) / duration, 1)
      setVal(Math.round(end * p))
      if (p < 1) raf = requestAnimationFrame(tick)
    }
    raf = requestAnimationFrame(tick)
    return () => cancelAnimationFrame(raf)
  }, [started, end, duration, reducedMotion])

  return (
    <span ref={ref} className={className}>
      {val}
      {suffix}
    </span>
  )
}
