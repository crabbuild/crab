"use client"

import { useCallback, useSyncExternalStore } from "react"
import { cn } from "@/lib/utils"

interface ScrollHeaderProps {
  children: React.ReactNode
  /**
   * Scroll position in CSS pixels at which the header transitions from
   * transparent to solid. Defaults to `window.innerHeight * 0.5` (roughly
   * the height of a typical hero section), with a hard floor of 200px so
   * short viewports still get a sensible threshold.
   */
  transitionThreshold?: number
  className?: string
}

/**
 * Sticky header wrapper that fades between a transparent background (when
 * the page is at the top) and a solid, blurred background (after the
 * visitor has scrolled past `transitionThreshold` pixels).
 *
 * Uses `requestAnimationFrame` to coalesce scroll and viewport changes.
 * With `prefers-reduced-motion`, the snapshot stays in the solid state.
 *
 * The CSS transition runs for 200ms (within the 150–300ms range
 * specified by the design system) on `background-color`, `backdrop-filter`,
 * and `border-color` so the wrapper composes cleanly with whatever
 * children are passed in (typically `<SiteHeader />`).
 */
function computeThreshold(transitionThreshold: number | undefined): number {
  if (typeof transitionThreshold === "number") {
    return transitionThreshold
  }
  // Half the viewport approximates "past the hero" without requiring
  // callers to measure their hero element. Floor at 200px so very short
  // viewports still pick up the transition.
  return Math.max(200, window.innerHeight * 0.5)
}

export function ScrollHeader({
  children,
  transitionThreshold,
  className,
}: ScrollHeaderProps) {
  const subscribe = useCallback((notify: () => void) => {
    const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)")
    let frame: number | null = null

    const schedule = () => {
      if (frame !== null) return
      frame = window.requestAnimationFrame(() => {
        frame = null
        notify()
      })
    }

    window.addEventListener("scroll", schedule, { passive: true })
    window.addEventListener("resize", schedule)
    reducedMotion.addEventListener("change", schedule)

    return () => {
      window.removeEventListener("scroll", schedule)
      window.removeEventListener("resize", schedule)
      reducedMotion.removeEventListener("change", schedule)
      if (frame !== null) window.cancelAnimationFrame(frame)
    }
  }, [])

  const snapshot = useCallback(
    () =>
      window.matchMedia("(prefers-reduced-motion: reduce)").matches ||
      window.scrollY > computeThreshold(transitionThreshold),
    [transitionThreshold]
  )
  const isScrolled = useSyncExternalStore(subscribe, snapshot, () => false)

  return (
    <div
      data-scrolled={isScrolled ? "true" : "false"}
      className={cn(
        "sticky top-0 z-40 w-full",
        // Transition both the background and the border so the header
        // settles into its solid state cleanly. Duration sits at 200ms
        // (mid-range of the 150–300ms band specified by the design).
        "transition-[background-color,backdrop-filter,border-color] duration-200 ease-out",
        // Border is only applied in the solid state; the transparent
        // state floats over the page without a visible divider.
        "border-b",
        isScrolled
          ? "border-border bg-background/80 backdrop-blur"
          : "border-transparent bg-background/90",
        className
      )}
    >
      {children}
    </div>
  )
}
