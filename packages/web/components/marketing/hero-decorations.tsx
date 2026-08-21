"use client"

import { useEffect, useRef, useState } from "react"

import { cn } from "@/lib/utils"

/**
 * Hook returning true when the user requests reduced motion.
 * Updates reactively if the OS-level preference changes mid-session.
 */
function usePrefersReducedMotion(): boolean {
  const [reduced, setReduced] = useState(false)

  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return
    const mq = window.matchMedia("(prefers-reduced-motion: reduce)")
    setReduced(mq.matches)
    const handler = (e: MediaQueryListEvent) => setReduced(e.matches)
    mq.addEventListener("change", handler)
    return () => mq.removeEventListener("change", handler)
  }, [])

  return reduced
}

/**
 * Parallax grid using --border lines at low opacity. Translates vertically
 * in proportion to scroll position. Disables the parallax transform under
 * prefers-reduced-motion.
 */
export function ParallaxGridBackground() {
  const reducedMotion = usePrefersReducedMotion()
  const [offset, setOffset] = useState(0)
  const ref = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (reducedMotion) return
    if (typeof window === "undefined") return

    let raf = 0
    let ticking = false

    const update = () => {
      const el = ref.current
      if (el) {
        const rect = el.getBoundingClientRect()
        // ~30% of scroll-into-section distance, subtle and unobtrusive.
        setOffset(-rect.top * 0.3)
      }
      ticking = false
    }

    const onScroll = () => {
      if (ticking) return
      ticking = true
      raf = window.requestAnimationFrame(update)
    }

    update()
    window.addEventListener("scroll", onScroll, { passive: true })
    return () => {
      window.removeEventListener("scroll", onScroll)
      if (raf) window.cancelAnimationFrame(raf)
    }
  }, [reducedMotion])

  return (
    <div
      ref={ref}
      aria-hidden="true"
      className="pointer-events-none absolute inset-0 overflow-hidden"
    >
      <div
        className="absolute inset-x-0 -top-32 -bottom-32"
        style={{
          backgroundImage:
            "linear-gradient(to right, color-mix(in oklab, var(--border) 60%, transparent) 1px, transparent 1px), linear-gradient(to bottom, color-mix(in oklab, var(--border) 60%, transparent) 1px, transparent 1px)",
          backgroundSize: "4rem 4rem",
          transform: reducedMotion ? "none" : `translate3d(0, ${offset}px, 0)`,
          willChange: reducedMotion ? "auto" : "transform",
          maskImage:
            "radial-gradient(ellipse at 50% 30%, black 40%, transparent 75%)",
          WebkitMaskImage:
            "radial-gradient(ellipse at 50% 30%, black 40%, transparent 75%)",
        }}
      />
    </div>
  )
}

/**
 * Types `text` character-by-character on mount (one-shot). Renders the full
 * text immediately when reduced motion is requested. Always exposes the full
 * text to assistive technology so screen readers see the headline at once.
 */
export function TypingHeadline({
  text,
  className,
}: {
  text: string
  className?: string
}) {
  const reducedMotion = usePrefersReducedMotion()
  const [shown, setShown] = useState(reducedMotion ? text.length : 0)

  useEffect(() => {
    if (reducedMotion) {
      setShown(text.length)
      return
    }
    setShown(0)
    let i = 0
    const interval = window.setInterval(() => {
      i += 1
      setShown(i)
      if (i >= text.length) window.clearInterval(interval)
    }, 35)
    return () => window.clearInterval(interval)
  }, [text, reducedMotion])

  const visible = text.slice(0, shown)
  const done = shown >= text.length

  return (
    <span className={cn("inline", className)}>
      <span aria-hidden="true">{visible}</span>
      <span className="sr-only">{text}</span>
      {!done && !reducedMotion && (
        <span
          aria-hidden="true"
          className="ml-1 inline-block w-[2px] -translate-y-1 align-middle bg-primary motion-safe:animate-pulse"
          style={{ height: "0.9em" }}
        />
      )}
    </span>
  )
}
