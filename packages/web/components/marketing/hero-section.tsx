import type { LucideIcon } from "lucide-react"
import Link from "next/link"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  ParallaxGridBackground,
  TypingHeadline,
} from "@/components/marketing/hero-decorations"
import { Reveal } from "@/components/marketing/reveal"
import { cn } from "@/lib/utils"

export type HeroAnimatedBackground = "grid" | "gradient-mesh" | "particles" | "none"
export type HeroHeadlineEffect = "gradient" | "typing" | "shimmer" | "none"

export interface HeroSectionProps {
  badge?: { text: string; dot?: boolean }
  headline: React.ReactNode
  subheadline: React.ReactNode
  primaryCTA?: { label: string; href: string; icon?: LucideIcon }
  secondaryCTA?: { label: string; href: string; icon?: LucideIcon }
  diagram?: React.ReactNode
  className?: string
  /**
   * Decorative animated background.
   * - 'grid': subtle parallax grid using --border lines.
   * - 'gradient-mesh': soft radial-gradient mesh using --primary / --primary-muted.
   * - 'particles': deterministic CSS-animated floating particle dots.
   * - 'none': no decorative background.
   * @default 'none'
   */
  animatedBackground?: HeroAnimatedBackground
  /**
   * Visual effect applied to the headline.
   * - 'gradient': linear-gradient text using --primary to a darker shade via bg-clip-text.
   * - 'typing': types the headline text character-by-character on mount (one-shot).
   *   The 'typing' effect requires `headline` to be a string; non-string ReactNodes
   *   render statically.
   * - 'shimmer': moving gradient highlight along the text.
   * - 'none': renders the headline as-is.
   * @default 'none'
   */
  headlineEffect?: HeroHeadlineEffect
}

/**
 * Static gradient mesh decoration using brand color tokens. Pure CSS — no
 * motion — so it is unaffected by prefers-reduced-motion.
 */
function GradientMeshBackground() {
  return (
    <div
      aria-hidden="true"
      className="pointer-events-none absolute inset-0 overflow-hidden"
    >
      <div
        className="absolute inset-0"
        style={{
          backgroundImage: [
            "radial-gradient(circle at 20% 20%, color-mix(in oklab, var(--primary) 22%, transparent), transparent 55%)",
            "radial-gradient(circle at 80% 30%, color-mix(in oklab, var(--primary-muted) 80%, transparent), transparent 60%)",
            "radial-gradient(circle at 50% 90%, color-mix(in oklab, var(--primary) 14%, transparent), transparent 65%)",
          ].join(", "),
        }}
      />
    </div>
  )
}

const PARTICLE_PRESETS = [
  { size: 6, left: 10, top: 20, delay: 0, duration: 8 },
  { size: 10, left: 25, top: 45, delay: 2, duration: 12 },
  { size: 4, left: 40, top: 15, delay: 1, duration: 7 },
  { size: 8, left: 60, top: 35, delay: 4, duration: 10 },
  { size: 5, left: 85, top: 25, delay: 3, duration: 9 },
  { size: 12, left: 75, top: 60, delay: 5, duration: 14 },
  { size: 7, left: 15, top: 75, delay: 1.5, duration: 11 },
  { size: 9, left: 50, top: 80, delay: 2.5, duration: 13 },
  { size: 4, left: 90, top: 70, delay: 0.5, duration: 8 },
  { size: 8, left: 30, top: 85, delay: 3.5, duration: 10 },
]

function ParticlesBackground() {
  return (
    <div
      aria-hidden="true"
      className="pointer-events-none absolute inset-0 overflow-hidden"
    >
      {PARTICLE_PRESETS.map((p, i) => (
        <div
          key={i}
          className="absolute rounded-full bg-primary/20 motion-safe:animate-[float-particle_10s_infinite_ease-in-out]"
          style={{
            width: `${p.size}px`,
            height: `${p.size}px`,
            left: `${p.left}%`,
            top: `${p.top}%`,
            animationDelay: `${p.delay}s`,
            animationDuration: `${p.duration}s`,
          }}
        />
      ))}
    </div>
  )
}

export function HeroSection({
  badge,
  headline,
  subheadline,
  primaryCTA,
  secondaryCTA,
  diagram,
  className,
  animatedBackground = "none",
  headlineEffect = "none",
}: HeroSectionProps) {
  const headlineNode =
    headlineEffect === "typing" && typeof headline === "string" ? (
      <TypingHeadline text={headline} />
    ) : (
      headline
    )

  const headlineClassName = cn(
    "text-4xl font-bold tracking-tight md:text-5xl lg:text-6xl p-2",
    headlineEffect === "gradient" || headlineEffect === "shimmer"
      ? "bg-clip-text text-transparent"
      : "text-foreground",
    headlineEffect === "shimmer" && "animate-shimmer"
  )

  const headlineStyle =
    headlineEffect === "gradient"
      ? {
          backgroundImage:
            "linear-gradient(135deg, var(--primary) 0%, color-mix(in oklab, var(--primary) 40%, var(--foreground)) 100%)",
        }
      : headlineEffect === "shimmer"
      ? {
          backgroundImage:
            "linear-gradient(90deg, var(--primary) 0%, var(--foreground) 25%, var(--primary) 50%, var(--foreground) 75%, var(--primary) 100%)",
        }
      : undefined

  return (
    <section
      className={cn(
        "relative overflow-hidden bg-background py-12 md:py-20",
        className,
      )}
    >
      {animatedBackground === "grid" && <ParallaxGridBackground />}
      {animatedBackground === "gradient-mesh" && <GradientMeshBackground />}
      {animatedBackground === "particles" && <ParticlesBackground />}

      <div className="relative mx-auto max-w-5xl px-6 text-center">
        {badge && (
          <Reveal>
            <div className="mb-6 inline-flex">
              <Badge variant="secondary">
                {badge.dot && (
                  <span
                    aria-hidden="true"
                    className="mr-1.5 inline-block size-2 rounded-full bg-primary"
                  />
                )}
                {badge.text}
              </Badge>
            </div>
          </Reveal>
        )}

        <Reveal>
          <h1 className={headlineClassName} style={headlineStyle}>
            {headlineNode}
          </h1>
        </Reveal>

        <Reveal>
          <p className="mx-auto mt-6 max-w-2xl text-lg text-muted-foreground">
            {subheadline}
          </p>
        </Reveal>

        {(primaryCTA || secondaryCTA) && (
          <Reveal>
            <div className="mt-10 flex flex-wrap items-center justify-center gap-4">
              {primaryCTA && (
                <Button
                  variant="default"
                  size="lg"
                  render={<Link href={primaryCTA.href} />}
                >
                  {primaryCTA.icon && <primaryCTA.icon />}
                  {primaryCTA.label}
                </Button>
              )}
              {secondaryCTA && (
                <Button
                  variant="outline"
                  size="lg"
                  render={<Link href={secondaryCTA.href} />}
                >
                  {secondaryCTA.icon && <secondaryCTA.icon />}
                  {secondaryCTA.label}
                </Button>
              )}
            </div>
          </Reveal>
        )}

        {diagram && (
          <Reveal>
            <div className="mt-16">{diagram}</div>
          </Reveal>
        )}
      </div>
    </section>
  )
}
