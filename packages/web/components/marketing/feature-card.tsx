import Link from "next/link"
import { ArrowRight, type LucideIcon } from "lucide-react"

import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { cn } from "@/lib/utils"

export interface FeatureCardProps {
  icon: LucideIcon
  title: string
  description: string
  /**
   * Optional destination for a "Learn more" link rendered at the bottom of
   * the card. When provided, a focusable `<a>` is rendered as the
   * keyboard-accessible affordance for the card.
   */
  href?: string
  iconSize?: number
  className?: string
}

export function FeatureCard({
  icon: Icon,
  title,
  description,
  href,
  iconSize = 20,
  className,
}: FeatureCardProps) {
  return (
    <Card
      className={cn(
        // Token-driven elevation: rests with --card-shadow, lifts to
        // --card-shadow-hover on hover. Transition uses --duration-normal
        // and --ease-out for consistency with the global motion system.
        "group/feature shadow-card transition-shadow duration-(--duration-normal) ease-(--ease-out-app) hover:shadow-card-hover glow-on-hover",
        className
      )}
    >
      <CardHeader>
        <div
          aria-hidden="true"
          className="mb-3 inline-flex h-10 w-10 items-center justify-center rounded-md bg-primary/10 text-primary transition-transform duration-(--duration-normal) ease-(--ease-out-app) group-hover/feature:scale-110"
        >
          <Icon size={iconSize} strokeWidth={2} />
        </div>
        <CardTitle>{title}</CardTitle>
        <CardDescription>{description}</CardDescription>
      </CardHeader>
      {href ? (
        <CardContent>
          <Link
            href={href}
            className={cn(
              "group/learn-more inline-flex items-center gap-1 text-xs font-medium text-primary",
              "transition-colors duration-(--duration-fast)",
              "hover:text-primary-hover",
              // Visible keyboard focus indicator. --ring is sky-500/sky-400,
              // which exceeds 3:1 contrast against both the white (light)
              // and slate-900 (dark) card surfaces.
              "rounded-sm outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-card"
            )}
          >
            <span>Learn more</span>
            <ArrowRight
              aria-hidden="true"
              size={14}
              strokeWidth={2}
              className="transition-transform duration-(--duration-fast) group-hover/learn-more:translate-x-0.5"
            />
          </Link>
        </CardContent>
      ) : null}
    </Card>
  )
}
