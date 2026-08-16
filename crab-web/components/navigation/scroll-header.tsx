"use client"

import { useEffect, useRef, useState } from "react"
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
 * Uses `requestAnimationFrame` to coalesce scroll events. Respects
 * `prefers-reduced-motion`: when set, the wrapper renders in the solid
 * state immediately and skips the scroll listener entirely.
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
  // Compute the initial scrolled state synchronously so a refresh
  // mid-page (or a user with reduced motion) doesn't flash the
  // transparent state on mount.
  const [isScrolled, setIsScrolled] = useState<boolean>(() => {
    if (typeof window === "undefined") return false
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      return true
    }
    return window.scrollY > computeThreshold(transitionThreshold)
  })
  const rafRef = useRef<number | null>(null)

  useEffect(() => {
    // When reduced motion is requested, the initializer already pinned
    // us to the solid state and we don't subscribe to scroll updates.
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      return
    }

    let threshold = computeThreshold(transitionThreshold)

    const update = () => {
      rafRef.current = null
      setIsScrolled(window.scrollY > threshold)
    }

    const onScroll = () => {
      if (rafRef.current !== null) return
      rafRef.current = window.requestAnimationFrame(update)
    }

    const onResize = () => {
      threshold = computeThreshold(transitionThreshold)
      onScroll()
    }

    window.addEventListener("scroll", onScroll, { passive: true })
    window.addEventListener("resize", onResize)

    return () => {
      window.removeEventListener("scroll", onScroll)
      window.removeEventListener("resize", onResize)
      if (rafRef.current !== null) {
        window.cancelAnimationFrame(rafRef.current)
      }
    }
  }, [transitionThreshold])

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
          ? "bg-background/80 backdrop-blur border-border"
          : "bg-background/90 border-transparent",
        className,
      )}
    >
      {children}
    </div>
  )
}
