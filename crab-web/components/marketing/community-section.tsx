import type { LucideIcon, LucideProps } from "lucide-react"
import { Heart, MessageCircle } from "lucide-react"
import { forwardRef } from "react"

// Github brand icon was removed in lucide-react v0.396+; inline SVG replacement
const Github = forwardRef<SVGSVGElement, LucideProps>(function Github(props, ref) {
  return (
    <svg
      ref={ref}
      xmlns="http://www.w3.org/2000/svg"
      width={24}
      height={24}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={2}
      strokeLinecap="round"
      strokeLinejoin="round"
      {...props}
    >
      <path d="M15 22v-4a4.8 4.8 0 0 0-1-3.5c3 0 6-2 6-5.5.08-1.25-.27-2.48-1-3.5.28-1.15.28-2.35 0-3.5 0 0-1 0-3 1.5-2.64-.5-5.36-.5-8 0C6 2 5 2 5 2c-.3 1.15-.3 2.35 0 3.5A5.403 5.403 0 0 0 4 9c0 3.5 3 5.5 6 5.5-.39.49-.68 1.05-.85 1.65-.17.6-.22 1.23-.15 1.85v4" />
      <path d="M9 18c-4.51 2-5-2-7-2" />
    </svg>
  )
}) as unknown as LucideIcon

import { Reveal } from "@/components/marketing/reveal"

/* ------------------------------------------------------------------ */
/*  Types                                                             */
/* ------------------------------------------------------------------ */

interface CommunityCard {
  icon: LucideIcon
  title: string
  description: string
  cta: { label: string; href: string }
}

/* ------------------------------------------------------------------ */
/*  Data                                                              */
/* ------------------------------------------------------------------ */

const cards: CommunityCard[] = [
  {
    icon: Github,
    title: "Open Source",
    description:
      "Crab is fully open source. Browse the code, report issues, and contribute on GitHub.",
    cta: { label: "Star on GitHub", href: "#" },
  },
  {
    icon: MessageCircle,
    title: "Join the Community",
    description:
      "Connect with other Crab users, ask questions, share tips, and get help from the team.",
    cta: { label: "Join Discord", href: "#" },
  },
  {
    icon: Heart,
    title: "Contribute",
    description:
      "From bug fixes to new features — we welcome contributions of all sizes. Read the guide to get started.",
    cta: { label: "Contributor Guide", href: "#" },
  },
]

/* ------------------------------------------------------------------ */
/*  Component                                                         */
/* ------------------------------------------------------------------ */

export function CommunitySection() {
  return (
    <section
      className="w-full px-6 py-section"
      aria-labelledby="community-heading"
    >
      <div className="mx-auto max-w-5xl">
        {/* ── Section header ── */}
        <Reveal>
          <div className="mx-auto mb-14 max-w-2xl text-center">
            <span className="inline-block rounded-full bg-primary-muted px-3 py-1 text-sm font-medium text-primary">
              Community
            </span>

            <h2
              id="community-heading"
              className="mt-4 font-heading text-heading-xl font-bold tracking-tight text-foreground md:text-heading-2xl"
            >
              Built in the open
            </h2>

            <p className="mt-3 text-lg text-muted-foreground">
              Crab is open source and community-driven. Join&nbsp;us.
            </p>
          </div>
        </Reveal>

        {/* ── Card grid ── */}
        <div className="grid gap-6 md:grid-cols-3">
          {cards.map((card) => (
            <Reveal key={card.title}>
              <article
                className="glass glow-on-hover flex h-full flex-col rounded-card p-card"
              >
                {/* Icon */}
                <div
                  className="mb-4 flex h-11 w-11 items-center justify-center rounded-full bg-primary-muted"
                  aria-hidden="true"
                >
                  <card.icon className="h-5 w-5 text-primary" />
                </div>

                {/* Title */}
                <h3 className="font-semibold text-foreground">{card.title}</h3>

                {/* Description */}
                <p className="mt-2 flex-1 text-sm leading-relaxed text-muted-foreground">
                  {card.description}
                </p>

                {/* CTA link */}
                <a
                  href={card.cta.href}
                  className="mt-4 inline-flex items-center gap-1 text-sm font-medium text-primary transition-colors duration-[var(--duration-fast)] hover:text-primary-hover"
                >
                  {card.cta.label}
                  <span aria-hidden="true" className="tracking-tight">
                    &nbsp;→
                  </span>
                </a>
              </article>
            </Reveal>
          ))}
        </div>
      </div>
    </section>
  )
}
