"use client"

import { Download, Pause, Play, RotateCcw } from "lucide-react"
import {
  type ComponentType,
  useEffect,
  useId,
  useRef,
  useState,
  useSyncExternalStore,
} from "react"

import {
  DedupMotionScene,
  type LaunchMotionStory,
  type MotionSceneProps,
  PublishMotionScene,
} from "@/components/blog/launch-motion-scenes"
import {
  GcMotionScene,
  HydrateMotionScene,
} from "@/components/blog/launch-motion-storage-scenes"
import { cn } from "@/lib/utils"

type StoryDefinition = {
  eyebrow: string
  title: string
  caption: string
  phases: readonly string[]
  gif: string
  svg: string
  Scene: ComponentType<MotionSceneProps>
}

const STORIES: Record<LaunchMotionStory, StoryDefinition> = {
  dedup: {
    eyebrow: "MOTION STUDY 01 · CHUNK IDENTITY",
    title: "A tiny edit should stay a tiny upload",
    caption:
      "Content-defined boundaries resynchronize after an edit. Earlier chunk identities stay reusable; only genuinely new chunk data enters a new xorb.",
    phases: [
      "Stream file",
      "Find boundaries",
      "Match identities",
      "Upload new bytes",
      "Seal xorb",
    ],
    gif: "/animations/crab-chunk-reuse.gif",
    svg: "/animations/crab-chunk-reuse.svg",
    Scene: DedupMotionScene,
  },
  publish: {
    eyebrow: "MOTION STUDY 02 · PUBLICATION",
    title: "Durable first. Visible second.",
    caption:
      "Git objects and Crab data prepare independently. The ref gate opens only after both immutable closures are durable and the expected-old comparison succeeds.",
    phases: [
      "Prepare",
      "Upload Git",
      "Upload data",
      "Compare ref",
      "Publish tip",
    ],
    gif: "/animations/crab-durable-publish.gif",
    svg: "/animations/crab-durable-publish.svg",
    Scene: PublishMotionScene,
  },
  hydrate: {
    eyebrow: "MOTION STUDY 03 · LAZY HYDRATION",
    title: "Read the ranges this job actually needs",
    caption:
      "A pointer resolves an ordered recipe, coalesces required ranges, reconstructs the file, and verifies its full identity before materializing it.",
    phases: [
      "Read pointer",
      "Resolve recipe",
      "Fetch ranges",
      "Reconstruct",
      "Verify file",
    ],
    gif: "/animations/crab-selective-hydration.gif",
    svg: "/animations/crab-selective-hydration.svg",
    Scene: HydrateMotionScene,
  },
  gc: {
    eyebrow: "MOTION STUDY 04 · SAFE GC",
    title: "Reachability decides what survives",
    caption:
      "The mark set protects live data. The grace window protects recent orphans. Only objects that are both old and unreachable become deletion candidates.",
    phases: [
      "Snapshot",
      "Walk roots",
      "Mark closure",
      "Apply grace",
      "Classify",
    ],
    gif: "/animations/crab-reachability-gc.gif",
    svg: "/animations/crab-reachability-gc.svg",
    Scene: GcMotionScene,
  },
}

const PHASE_DURATION_MS = 1_350

function subscribeToReducedMotion(update: () => void) {
  const media = window.matchMedia("(prefers-reduced-motion: reduce)")
  media.addEventListener("change", update)
  return () => media.removeEventListener("change", update)
}

function usePrefersReducedMotion() {
  return useSyncExternalStore(
    subscribeToReducedMotion,
    () => window.matchMedia("(prefers-reduced-motion: reduce)").matches,
    () => true
  )
}

export function LaunchMotionFigure({
  story: storyName,
}: {
  story: LaunchMotionStory
}) {
  const story = STORIES[storyName]
  const figureRef = useRef<HTMLElement>(null)
  const generatedId = useId().replaceAll(":", "")
  const [phase, setPhase] = useState(0)
  const [paused, setPaused] = useState(false)
  const [inView, setInView] = useState(false)
  const [hasInteracted, setHasInteracted] = useState(false)
  const reducedMotion = usePrefersReducedMotion()
  const shouldAnimate = inView && !paused && !reducedMotion
  const Scene = story.Scene

  useEffect(() => {
    const figure = figureRef.current
    if (!figure) return

    const observer = new IntersectionObserver(
      ([entry]) => setInView(entry.isIntersecting),
      { threshold: 0.2 }
    )
    observer.observe(figure)
    return () => observer.disconnect()
  }, [])

  useEffect(() => {
    if (!shouldAnimate) return
    const timer = window.setTimeout(() => {
      setPhase((current) => (current + 1) % story.phases.length)
    }, PHASE_DURATION_MS)
    return () => window.clearTimeout(timer)
  }, [phase, shouldAnimate, story.phases.length])

  const goToPhase = (nextPhase: number) => {
    setHasInteracted(true)
    setPhase(nextPhase)
  }

  return (
    <figure
      ref={figureRef}
      className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.5rem] border border-slate-700 bg-[#07101b] text-slate-100 shadow-[0_28px_90px_rgba(2,8,18,0.28)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]"
    >
      <header className="flex flex-col gap-4 border-b border-slate-800 bg-[#0a1522] px-4 py-4 sm:flex-row sm:items-end sm:justify-between sm:px-6 sm:py-5">
        <div>
          <p className="m-0 font-mono text-[10px] font-bold tracking-[0.19em] text-cyan-300">
            {story.eyebrow}
          </p>
          <h3 className="m-0 mt-1.5 text-lg font-semibold tracking-[-0.025em] text-white sm:text-xl">
            {story.title}
          </h3>
        </div>
        <div className="flex items-center gap-2">
          <a
            href={story.svg}
            download
            className="inline-flex min-h-10 items-center gap-2 rounded-full border border-slate-700 px-3 font-mono text-[10px] font-bold tracking-[0.08em] text-slate-400 transition-colors hover:border-slate-500 hover:text-white focus-visible:ring-2 focus-visible:ring-cyan-300 focus-visible:outline-none"
          >
            <Download size={13} aria-hidden="true" />
            SVG
          </a>
          <a
            href={story.gif}
            download
            className="inline-flex min-h-10 items-center gap-2 rounded-full border border-slate-700 px-3 font-mono text-[10px] font-bold tracking-[0.08em] text-slate-400 transition-colors hover:border-slate-500 hover:text-white focus-visible:ring-2 focus-visible:ring-cyan-300 focus-visible:outline-none"
          >
            <Download size={13} aria-hidden="true" />
            GIF
          </a>
          <button
            type="button"
            onClick={() => {
              setHasInteracted(true)
              setPaused((current) => !current)
            }}
            disabled={reducedMotion}
            className="inline-flex min-h-10 min-w-24 items-center justify-center gap-2 rounded-full border border-cyan-300/40 bg-cyan-300/10 px-4 font-mono text-[10px] font-bold tracking-[0.08em] text-cyan-200 transition-colors hover:bg-cyan-300/15 focus-visible:ring-2 focus-visible:ring-cyan-300 focus-visible:outline-none disabled:cursor-not-allowed disabled:border-slate-700 disabled:bg-slate-900 disabled:text-slate-500"
            aria-label={paused ? "Play SVG animation" : "Pause SVG animation"}
          >
            {paused || reducedMotion ? (
              <Play size={13} aria-hidden="true" />
            ) : (
              <Pause size={13} aria-hidden="true" />
            )}
            {reducedMotion ? "MANUAL" : paused ? "PLAY" : "PAUSE"}
          </button>
          <button
            type="button"
            onClick={() => goToPhase(0)}
            className="flex size-10 items-center justify-center rounded-full border border-slate-700 text-slate-400 transition-colors hover:border-slate-500 hover:text-white focus-visible:ring-2 focus-visible:ring-cyan-300 focus-visible:outline-none"
            aria-label="Restart SVG animation"
          >
            <RotateCcw size={14} aria-hidden="true" />
          </button>
        </div>
      </header>

      <div
        className="[scrollbar-width:thin] [scrollbar-color:#334155_#07101b] overflow-x-auto bg-[#07101b]"
        role="region"
        aria-label={`${story.title} animated SVG`}
        tabIndex={0}
      >
        <Scene
          phase={phase}
          id={`${storyName}-${generatedId}`}
          animate={shouldAnimate}
        />
      </div>

      <div className="border-t border-slate-800 bg-[#0a1522] px-4 py-4 sm:px-6">
        <div
          className="grid grid-cols-5 gap-1.5"
          role="tablist"
          aria-label={`${story.title} animation phases`}
        >
          {story.phases.map((label, index) => {
            const active = phase === index
            const complete = phase > index
            return (
              <button
                key={label}
                type="button"
                role="tab"
                aria-selected={active}
                onClick={() => goToPhase(index)}
                className="group min-w-0 rounded-md py-1.5 text-left focus-visible:ring-2 focus-visible:ring-cyan-300 focus-visible:outline-none"
              >
                <span
                  className={cn(
                    "block h-1 rounded-full transition-colors",
                    active
                      ? "bg-cyan-300"
                      : complete
                        ? "bg-emerald-400/60"
                        : "bg-slate-700 group-hover:bg-slate-600"
                  )}
                />
                <span
                  className={cn(
                    "mt-2 hidden truncate font-mono text-[9px] font-bold tracking-[0.05em] sm:block",
                    active ? "text-cyan-200" : "text-slate-500"
                  )}
                >
                  {label.toUpperCase()}
                </span>
              </button>
            )
          })}
        </div>
        <p
          className="m-0 mt-3 text-xs font-medium text-cyan-200 sm:hidden"
          aria-live={hasInteracted ? "polite" : "off"}
        >
          {phase + 1}. {story.phases[phase]}
        </p>
      </div>

      <figcaption className="border-t border-slate-800 bg-[#07101b] px-4 py-3 text-xs leading-5 text-slate-500 sm:px-6">
        {story.caption}
      </figcaption>
    </figure>
  )
}
