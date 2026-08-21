import type { LucideIcon } from "lucide-react"
import Link from "next/link"

import { Button } from "@/components/ui/button"
import { cn } from "@/lib/utils"

export interface CTASectionProps {
  headline: string
  description: string
  primaryCTA: { label: string; href: string; icon?: LucideIcon }
  secondaryCTA?: { label: string; href: string; icon?: LucideIcon }
  variant?: "default" | "accent"
}

export function CTASection({
  headline,
  description,
  primaryCTA,
  secondaryCTA,
  variant = "default",
}: CTASectionProps) {
  return (
    <section
      className={cn(
        "w-full border-y px-6 py-16 md:py-24",
        variant === "default" && "bg-muted",
        variant === "accent" && "border-primary/20 bg-primary/5"
      )}
    >
      <div className="mx-auto max-w-3xl text-center">
        <h2 className="text-3xl font-bold tracking-tight text-foreground md:text-4xl">
          {headline}
        </h2>
        <p className="mt-4 text-lg text-muted-foreground">{description}</p>
        <div className="mt-8 flex flex-wrap items-center justify-center gap-4">
          <Button variant="default" size="lg" render={<Link href={primaryCTA.href} />}>
            {primaryCTA.icon && <primaryCTA.icon />}
            {primaryCTA.label}
          </Button>
          {secondaryCTA && (
            <Button variant="outline" size="lg" render={<Link href={secondaryCTA.href} />}>
              {secondaryCTA.icon && <secondaryCTA.icon />}
              {secondaryCTA.label}
            </Button>
          )}
        </div>
      </div>
    </section>
  )
}
