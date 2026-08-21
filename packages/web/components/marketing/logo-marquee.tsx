"use client"

import { cn } from "@/lib/utils"

/* ─── Types ─── */

export interface LogoMarqueeItem {
  /** Platform / integration name */
  name: string
  /** Short subtitle displayed below the name */
  sub: string
}

export interface LogoMarqueeProps {
  /** Override the default set of logos */
  items?: LogoMarqueeItem[]
  className?: string
}

/* ─── Default data ─── */

const defaultItems: LogoMarqueeItem[] = [
  { name: "AWS S3", sub: "Object storage" },
  { name: "Google Cloud", sub: "GCS" },
  { name: "Azure", sub: "Blob storage" },
  { name: "GitHub", sub: "Refs & releases" },
  { name: "GitLab", sub: "Self-hosted CI" },
  { name: "DVC", sub: "Pipeline import" },
]

/* ─── Single logo pill ─── */

function LogoPill({ name, sub }: LogoMarqueeItem) {
  return (
    <div
      className={cn(
        "flex shrink-0 items-center gap-3 rounded-full",
        "border border-border bg-card px-5 py-2.5",
        "transition-shadow duration-(--duration-normal) ease-(--ease-out-app)",
        "hover:shadow-card-hover",
      )}
    >
      {/* Icon placeholder — a branded circle with the first letter */}
      <span
        aria-hidden="true"
        className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-primary-muted text-sm font-semibold text-primary"
      >
        {name.charAt(0)}
      </span>
      <span className="flex flex-col leading-tight">
        <span className="text-sm font-semibold text-foreground">{name}</span>
        <span className="text-xs text-muted-foreground">{sub}</span>
      </span>
    </div>
  )
}

/* ─── Marquee component ─── */

/**
 * Infinite-scroll logo marquee for the "Trusted By" section.
 *
 * How it works:
 * - The item list is rendered **twice** side-by-side so the first copy
 *   seamlessly fills the gap left by the second copy scrolling away.
 * - The `.animate-marquee` CSS class (defined in globals.css) applies a
 *   `translateX(0 → -50%)` animation. Because the total content is exactly
 *   2× the single-run width, the loop is seamless.
 * - `prefers-reduced-motion: reduce` disables the animation via the
 *   global CSS rule, so both copies simply render side-by-side as a
 *   static strip.
 * - Gradient masks on left/right edges prevent a hard visual cut-off.
 */
export function LogoMarquee({ items = defaultItems, className }: LogoMarqueeProps) {
  return (
    <section
      aria-label="Trusted by leading platforms"
      className={cn("w-full py-section", className)}
    >
      {/* Section heading */}
      <p className="mb-8 text-center text-sm font-medium uppercase tracking-widest text-muted-foreground">
        Trusted by teams using
      </p>

      {/* Marquee viewport — gradient masks fade the left/right edges */}
      <div
        className="relative overflow-hidden"
        style={{
          maskImage:
            "linear-gradient(to right, transparent, black 8%, black 92%, transparent)",
          WebkitMaskImage:
            "linear-gradient(to right, transparent, black 8%, black 92%, transparent)",
        }}
      >
        {/* Scrolling track — duplicated children for seamless loop */}
        <div className="animate-marquee flex w-max gap-6">
          {/* First copy */}
          {items.map((item) => (
            <LogoPill key={`a-${item.name}`} {...item} />
          ))}
          {/* Second copy (duplicate for infinite loop) */}
          {items.map((item) => (
            <LogoPill key={`b-${item.name}`} {...item} />
          ))}
        </div>
      </div>
    </section>
  )
}
