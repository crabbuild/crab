"use client"

import { useRef, useEffect, useCallback } from "react"
import { cn } from "@/lib/utils"

interface ResponsiveTableWrapperProps {
  children: React.ReactNode
  className?: string
}

/**
 * Wraps a table in a horizontally scrollable container with a trailing
 * gradient fade that appears when content overflows on mobile.
 */
export function ResponsiveTableWrapper({
  children,
  className,
}: ResponsiveTableWrapperProps) {
  const ref = useRef<HTMLDivElement>(null)

  const updateOverflowState = useCallback(() => {
    const el = ref.current
    if (!el) return

    const isOverflowing = el.scrollWidth > el.clientWidth
    el.classList.toggle("is-overflowing", isOverflowing)

    const isScrolledEnd =
      isOverflowing &&
      Math.abs(el.scrollWidth - el.clientWidth - el.scrollLeft) < 2
    el.classList.toggle("is-scrolled-end", isScrolledEnd)
  }, [])

  useEffect(() => {
    const el = ref.current
    if (!el) return

    updateOverflowState()

    el.addEventListener("scroll", updateOverflowState, { passive: true })
    const resizeObserver = new ResizeObserver(updateOverflowState)
    resizeObserver.observe(el)

    return () => {
      el.removeEventListener("scroll", updateOverflowState)
      resizeObserver.disconnect()
    }
  }, [updateOverflowState])

  return (
    <div ref={ref} className={cn("table-scroll-wrapper", className)}>
      {children}
    </div>
  )
}
