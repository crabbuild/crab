"use client"

import { useCallback, useEffect, useRef, useState } from "react"
import { Quote } from "lucide-react"

import { cn } from "@/lib/utils"
import { Reveal } from "@/components/marketing/reveal"

/* ------------------------------------------------------------------ */
/*  Types                                                              */
/* ------------------------------------------------------------------ */

interface Testimonial {
  quote: string
  name: string
  role: string
  company: string
  initials: string
}

/* ------------------------------------------------------------------ */
/*  Data                                                               */
/* ------------------------------------------------------------------ */

const testimonials: Testimonial[] = [
  {
    quote:
      "Chunk-level dedup turned our 1,212 GB checkpoint repo into a 10 GB push. We swapped Git LFS for Crab in an afternoon and never looked back — pulls are instant and the bucket bill dropped by an order of magnitude.",
    name: "Maya Okonkwo",
    role: "Staff ML Engineer",
    company: "Drifthouse Robotics",
    initials: "MO",
  },
  {
    quote:
      "We version 500+ SafeTensors models across 8 teams. Crab's 3-tier dedup means each weekly fine-tune push is under 2 GB instead of the full 40 GB. Our cloud storage bill dropped 80%.",
    name: "Jonas Lindström",
    role: "Platform Lead",
    company: "NordAI Labs",
    initials: "JL",
  },
  {
    quote:
      "The FUSE mount is a game-changer. Our data scientists browse terabyte datasets as if they're local folders — only the chunks they actually read get downloaded. Training pipeline startup went from 45 minutes to under 2.",
    name: "Priya Mehta",
    role: "Head of MLOps",
    company: "Canopy Health AI",
    initials: "PM",
  },
]

const AUTO_ROTATE_MS = 6_000

/* ------------------------------------------------------------------ */
/*  Hook: prefers-reduced-motion                                       */
/* ------------------------------------------------------------------ */

function usePrefersReducedMotion(): boolean {
  const [reduced, setReduced] = useState(false)

  useEffect(() => {
    const mql = window.matchMedia("(prefers-reduced-motion: reduce)")
    setReduced(mql.matches)

    const handler = (e: MediaQueryListEvent) => setReduced(e.matches)
    mql.addEventListener("change", handler)
    return () => mql.removeEventListener("change", handler)
  }, [])

  return reduced
}

/* ------------------------------------------------------------------ */
/*  Component                                                          */
/* ------------------------------------------------------------------ */

export function TestimonialsCarousel() {
  const [activeIndex, setActiveIndex] = useState(0)
  const [isPaused, setIsPaused] = useState(false)
  const reducedMotion = usePrefersReducedMotion()
  const timerRef = useRef<ReturnType<typeof setInterval> | null>(null)

  /* Auto-rotation --------------------------------------------------- */
  const startTimer = useCallback(() => {
    timerRef.current = setInterval(() => {
      setActiveIndex((prev) => (prev + 1) % testimonials.length)
    }, AUTO_ROTATE_MS)
  }, [])

  const stopTimer = useCallback(() => {
    if (timerRef.current) {
      clearInterval(timerRef.current)
      timerRef.current = null
    }
  }, [])

  useEffect(() => {
    if (reducedMotion || isPaused) {
      stopTimer()
      return
    }
    startTimer()
    return stopTimer
  }, [isPaused, reducedMotion, startTimer, stopTimer])

  /* Jump to a specific testimonial via dot -------------------------  */
  const goTo = (index: number) => {
    setActiveIndex(index)
    // Reset the interval so the full 6 s starts from the click
    stopTimer()
    if (!isPaused && !reducedMotion) startTimer()
  }

  /* ---------------------------------------------------------------- */
  /*  Render                                                           */
  /* ---------------------------------------------------------------- */

  return (
    <section
      className="w-full px-6 py-section"
      aria-label="Customer testimonials"
    >
      <div className="mx-auto max-w-3xl text-center">
        {/* Section header */}
        <Reveal>
          <span className="inline-block rounded-full border border-primary/30 bg-primary-muted px-3 py-1 text-xs font-medium tracking-wide text-primary">
            Testimonials
          </span>
          <h2 className="mt-4 font-heading text-heading-xl font-bold tracking-tight text-foreground md:text-heading-2xl">
            Trusted by ML teams shipping at scale
          </h2>
        </Reveal>

        {/* Carousel area */}
        <div
          className="relative mt-12"
          onMouseEnter={() => setIsPaused(true)}
          onMouseLeave={() => setIsPaused(false)}
        >
          {/* Testimonial cards — stacked with crossfade */}
          <div
            className="relative"
            // Set a min-height so the container doesn't collapse during fade
            style={{ minHeight: "280px" }}
          >
            {testimonials.map((t, i) => {
              const isActive = i === activeIndex

              return (
                <div
                  key={t.initials}
                  role="group"
                  aria-roledescription="slide"
                  aria-label={`Testimonial ${i + 1} of ${testimonials.length}: ${t.name}`}
                  aria-hidden={!isActive}
                  className={cn(
                    "absolute inset-0 flex flex-col items-center",
                    /* Transition: crossfade + subtle vertical shift */
                    !reducedMotion &&
                      "transition-all duration-500 ease-out-app",
                    isActive
                      ? "pointer-events-auto opacity-100 translate-y-0"
                      : "pointer-events-none opacity-0 translate-y-3",
                  )}
                >
                  {/* Card */}
                  <blockquote
                    className={cn(
                      "rounded-card border border-border bg-card p-card shadow-card",
                      "flex w-full flex-col items-center gap-5",
                    )}
                  >
                    {/* Quote icon */}
                    <Quote
                      className="size-8 text-primary/70"
                      aria-hidden="true"
                    />

                    {/* Quote text */}
                    <p className="text-lg italic leading-relaxed text-foreground">
                      &ldquo;{t.quote}&rdquo;
                    </p>

                    {/* Attribution footer */}
                    <footer className="mt-2 flex items-center gap-3">
                      {/* Avatar circle */}
                      <div
                        aria-hidden="true"
                        className="flex size-10 shrink-0 items-center justify-center rounded-full bg-primary-muted text-sm font-semibold text-primary"
                      >
                        {t.initials}
                      </div>

                      <div className="text-left text-sm">
                        <cite className="not-italic font-semibold text-foreground">
                          {t.name}
                        </cite>
                        <p className="text-muted-foreground">
                          {t.role}, {t.company}
                        </p>
                      </div>
                    </footer>
                  </blockquote>
                </div>
              )
            })}
          </div>

          {/* Dot navigation */}
          <nav
            className="mt-8 flex items-center justify-center gap-2"
            aria-label="Testimonial navigation"
          >
            {testimonials.map((t, i) => (
              <button
                key={t.initials}
                type="button"
                onClick={() => goTo(i)}
                aria-label={`Go to testimonial by ${t.name}`}
                aria-current={i === activeIndex ? "true" : undefined}
                className={cn(
                  "size-2.5 rounded-full transition-all",
                  !reducedMotion && "duration-300 ease-out-app",
                  i === activeIndex
                    ? "scale-125 bg-primary"
                    : "bg-border hover:bg-muted-foreground/40",
                )}
              />
            ))}
          </nav>
        </div>
      </div>
    </section>
  )
}
