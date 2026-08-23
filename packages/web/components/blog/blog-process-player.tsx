"use client"

import {
  ChevronLeft,
  ChevronRight,
  GitBranch,
  LockKeyhole,
  Network,
} from "lucide-react"
import {
  type KeyboardEvent as ReactKeyboardEvent,
  useCallback,
  useRef,
  useState,
} from "react"

import { cn } from "@/lib/utils"

type StoryName = "integration" | "pipeline" | "locking"
type Tone = "git" | "data" | "control" | "store" | "safe" | "danger"

type Stage = {
  label: string
  title: string
  description: string
  invariant: string
  tone: Tone
  active: number[]
}

type Story = {
  eyebrow: string
  title: string
  caption: string
  nodes: string[]
  stages: Stage[]
}

const COLORS: Record<Tone, string> = {
  git: "#f97316",
  data: "#06b6d4",
  control: "#a78bfa",
  store: "#38bdf8",
  safe: "#34d399",
  danger: "#fb7185",
}

const STORIES: Record<StoryName, Story> = {
  integration: {
    eyebrow: "GIT EXTENSION TRACE",
    title: "Two Git hooks, one backed pointer contract",
    caption:
      "Select an operation to see whether Git invokes Crab for content transformation, remote transport, or both.",
    nodes: [
      "Worktree",
      "Git",
      "Filter process",
      "Staging",
      "Remote helper",
      "Object store",
    ],
    stages: [
      {
        label: "ADD",
        title: "Git starts the clean filter",
        description:
          "Git streams a tracked file through the long-running filter process.",
        invariant: "Path selection comes from .gitattributes.",
        tone: "git",
        active: [0, 1, 2],
      },
      {
        label: "STAGE",
        title: "Crab stages chunks before returning a pointer",
        description:
          "The filter hashes and chunks the bytes, then closes the staging boundary.",
        invariant: "No pointer is emitted without local reconstruction data.",
        tone: "data",
        active: [2, 3],
      },
      {
        label: "COMMIT",
        title: "Git records the pointer blob",
        description:
          "The commit stores a normal Git blob whose content is the Crab pointer.",
        invariant: "Git owns history; Crab owns pointer reconstruction.",
        tone: "git",
        active: [1, 2],
      },
      {
        label: "PUSH",
        title: "Git starts the remote helper",
        description:
          "The helper discovers pointer dependencies and transfers Git and Crab data.",
        invariant: "Helper stdout remains Git protocol output.",
        tone: "control",
        active: [1, 3, 4, 5],
      },
      {
        label: "CHECKOUT",
        title: "The smudge path resolves pointer bytes",
        description:
          "The filter can preserve the pointer or hydrate verified file content.",
        invariant: "Unverified partial content never reaches the worktree.",
        tone: "safe",
        active: [5, 2, 1, 0],
      },
    ],
  },
  pipeline: {
    eyebrow: "PUSH TRANSACTION TRACE",
    title: "Fourteen stages end at one visibility change",
    caption:
      "The first thirteen stages prepare or prove immutable dependencies. The ref transaction is the publication point.",
    nodes: ["Discover", "Lock", "Git pack", "Xorbs", "Shards", "Ref journal"],
    stages: [
      {
        label: "01",
        title: "Resolve the ref edit",
        description:
          "Validate source, destination, and expected old object ID.",
        invariant: "Malformed edits fail before upload.",
        tone: "control",
        active: [0],
      },
      {
        label: "02",
        title: "Acquire the destination lock",
        description: "Serialize writers that target the same ref.",
        invariant: "Every acquired lock is released.",
        tone: "control",
        active: [0, 1],
      },
      {
        label: "03",
        title: "Discover reachable Git objects",
        description: "Walk commits, trees, ordinary blobs, and pointer blobs.",
        invariant: "Discovery covers the complete Git closure.",
        tone: "git",
        active: [0, 2],
      },
      {
        label: "04",
        title: "Resolve every Crab pointer",
        description: "Match each pointer to staged or remotely proven content.",
        invariant: "A missing recipe rejects the push.",
        tone: "data",
        active: [0, 3],
      },
      {
        label: "05",
        title: "Classify chunks",
        description: "Separate known remote chunks from new content.",
        invariant: "Only proven remote chunks may be skipped.",
        tone: "data",
        active: [3],
      },
      {
        label: "06",
        title: "Build the Git pack",
        description: "Pack the reachable Git objects for immutable upload.",
        invariant: "The pack remains standard Git data.",
        tone: "git",
        active: [2],
      },
      {
        label: "07",
        title: "Pack new chunks into xorbs",
        description:
          "Compress new chunk sequences into content-addressed objects.",
        invariant: "Staged xorbs flush before publication.",
        tone: "data",
        active: [3],
      },
      {
        label: "08",
        title: "Upload immutable objects",
        description:
          "Transfer the Git pack and xorbs with bounded concurrency.",
        invariant: "Interrupted uploads may leave only safe orphans.",
        tone: "store",
        active: [2, 3],
      },
      {
        label: "09",
        title: "Build shard metadata",
        description: "Record every reconstruction term for each file version.",
        invariant: "Shard terms cover every chunk.",
        tone: "data",
        active: [3, 4],
      },
      {
        label: "10",
        title: "Upload shards",
        description: "Publish immutable reconstruction metadata.",
        invariant: "Shards never point at missing xorbs.",
        tone: "store",
        active: [4],
      },
      {
        label: "11",
        title: "Verify Git connectivity",
        description: "Prove the new commit graph is complete at the origin.",
        invariant: "A disconnected pack cannot become visible.",
        tone: "git",
        active: [2, 5],
      },
      {
        label: "12",
        title: "Verify pointer closure",
        description:
          "Prove every pointer resolves through durable shards and xorbs.",
        invariant: "Canonical storage, not cache hints, supplies proof.",
        tone: "data",
        active: [3, 4, 5],
      },
      {
        label: "13",
        title: "Compare expected old state",
        description: "Reject a stale writer if another push changed the ref.",
        invariant: "Concurrent writers cannot silently overwrite work.",
        tone: "danger",
        active: [1, 5],
      },
      {
        label: "14",
        title: "Commit the ref transaction",
        description: "Append the journal entry that makes the new tip visible.",
        invariant:
          "All immutable dependencies are durable before the ref moves.",
        tone: "safe",
        active: [4, 5],
      },
    ],
  },
  locking: {
    eyebrow: "CONCURRENT PUSH TRACE",
    title: "The lock saves work; expected-old protects history",
    caption:
      "Step through two same-ref pushes. The lock narrows concurrency, while the ref transaction decides correctness.",
    nodes: [
      "Alice",
      "Ref lock",
      "Object store",
      "Expected old",
      "Ref journal",
      "Bob",
    ],
    stages: [
      {
        label: "ARRIVE",
        title: "Two writers target main",
        description:
          "Alice and Bob both prepare edits against the same branch tip.",
        invariant: "Different refs remain independent.",
        tone: "git",
        active: [0, 5],
      },
      {
        label: "LOCK",
        title: "Alice acquires the ref lock",
        description: "Bob waits before performing contested publication work.",
        invariant: "Lock ownership is scoped to one ref.",
        tone: "control",
        active: [0, 1, 5],
      },
      {
        label: "UPLOAD",
        title: "Alice uploads immutable dependencies",
        description:
          "Git packs, xorbs, and shards can upload without changing visibility.",
        invariant: "Readers still see the old ref.",
        tone: "store",
        active: [0, 1, 2],
      },
      {
        label: "COMPARE",
        title: "Alice proves the expected old tip",
        description:
          "The ref transaction checks that main still names Alice's base commit.",
        invariant: "A stale expected value rejects the transaction.",
        tone: "control",
        active: [1, 3, 4],
      },
      {
        label: "COMMIT",
        title: "Alice publishes and releases",
        description:
          "The journal commits Alice's new tip, then the lock is released.",
        invariant: "Dependencies are durable before visibility changes.",
        tone: "safe",
        active: [0, 1, 4],
      },
      {
        label: "REPLAN",
        title: "Bob observes the new branch tip",
        description:
          "Bob reacquires the lock and compares against current state.",
        invariant: "Bob cannot publish an edit based on the old tip.",
        tone: "danger",
        active: [1, 3, 4, 5],
      },
      {
        label: "RETRY",
        title: "Bob rebases and retries",
        description:
          "Previously uploaded immutable data can be reused during the new plan.",
        invariant: "The retry publishes only after a fresh closure proof.",
        tone: "safe",
        active: [2, 3, 4, 5],
      },
    ],
  },
}

export function BlogProcessPlayer({ story: storyName }: { story: StoryName }) {
  const story = STORIES[storyName]
  const tabRefs = useRef<Array<HTMLButtonElement | null>>([])
  const [activeIndex, setActiveIndex] = useState(0)
  const stage = story.stages[activeIndex]
  const color = COLORS[stage.tone]
  const panelId = `${storyName}-process-panel`

  const goTo = useCallback(
    (index: number) => {
      setActiveIndex((index + story.stages.length) % story.stages.length)
    },
    [story.stages.length]
  )

  const moveTabFocus = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    let nextIndex: number | undefined
    if (event.key === "ArrowRight") nextIndex = activeIndex + 1
    if (event.key === "ArrowLeft") nextIndex = activeIndex - 1
    if (event.key === "Home") nextIndex = 0
    if (event.key === "End") nextIndex = story.stages.length - 1
    if (nextIndex === undefined) return

    event.preventDefault()
    const normalized = (nextIndex + story.stages.length) % story.stages.length
    goTo(normalized)
    tabRefs.current[normalized]?.focus()
  }

  const Icon =
    storyName === "locking"
      ? LockKeyhole
      : storyName === "integration"
        ? Network
        : GitBranch

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(62rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-xl border border-slate-800 bg-[#070b12] shadow-[0_24px_80px_rgba(2,6,23,0.24)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(62rem,calc(100vw-2rem))] lg:w-[min(62rem,calc(100vw-24.5rem))]">
      <header className="flex items-start gap-3 border-b border-slate-800 px-4 py-4 sm:px-5">
        <span className="mt-0.5 flex size-8 shrink-0 items-center justify-center rounded-md border border-slate-700 bg-slate-900 text-sky-300">
          <Icon size={16} aria-hidden="true" />
        </span>
        <div>
          <p className="m-0 font-mono text-[10px] tracking-[0.15em] text-slate-500">
            {story.eyebrow}
          </p>
          <h3 className="m-0 mt-1 text-base font-semibold text-slate-100">
            {story.title}
          </h3>
        </div>
      </header>

      <div
        className="overflow-x-auto border-b border-slate-800 bg-[#0b111a]"
        role="tablist"
        aria-label={`${story.title} stages`}
        onKeyDown={moveTabFocus}
      >
        <div className="flex min-w-max p-2">
          {story.stages.map((item, index) => {
            const active = index === activeIndex
            return (
              <button
                key={`${item.label}-${item.title}`}
                ref={(node) => {
                  tabRefs.current[index] = node
                }}
                id={`${storyName}-process-stage-${index}`}
                type="button"
                role="tab"
                aria-selected={active}
                aria-controls={panelId}
                tabIndex={active ? 0 : -1}
                onClick={() => goTo(index)}
                className={cn(
                  "min-h-11 rounded-md border px-3 font-mono text-[10px] font-semibold tracking-wide transition-colors focus-visible:ring-2 focus-visible:ring-sky-400/70 focus-visible:outline-none",
                  active
                    ? "border-slate-600 bg-slate-800 text-slate-100"
                    : "border-transparent text-slate-500 hover:bg-slate-900 hover:text-slate-300"
                )}
                style={active ? { borderColor: color, color } : undefined}
              >
                {item.label}
              </button>
            )
          })}
        </div>
      </div>

      <div className="overflow-x-auto p-4 sm:p-5">
        <div className="grid min-w-[48rem] grid-cols-[repeat(6,minmax(0,1fr))] items-center gap-4">
          {story.nodes.map((node, index) => {
            const active = stage.active.includes(index)
            return (
              <div key={node} className="relative flex items-center">
                <div
                  className={cn(
                    "flex min-h-20 w-full items-center justify-center rounded-lg border bg-slate-950 px-2 text-center text-sm font-medium transition-[border-color,background-color,opacity] duration-200",
                    active
                      ? "text-slate-100 opacity-100"
                      : "border-slate-800 text-slate-500 opacity-45"
                  )}
                  style={
                    active
                      ? { borderColor: color, backgroundColor: `${color}12` }
                      : undefined
                  }
                >
                  {node}
                </div>
                {index < story.nodes.length - 1 ? (
                  <span
                    className="absolute -right-4 h-px w-4 bg-slate-700"
                    aria-hidden="true"
                  />
                ) : null}
              </div>
            )
          })}
        </div>
      </div>

      <div
        id={panelId}
        role="tabpanel"
        aria-labelledby={`${storyName}-process-stage-${activeIndex}`}
        className="grid border-t border-slate-800 bg-[#0b111a] md:grid-cols-[1fr_auto]"
      >
        <div className="min-h-40 px-4 py-5 sm:px-5">
          <div className="flex items-center gap-2">
            <span className="font-mono text-[10px] text-slate-500">
              {String(activeIndex + 1).padStart(2, "0")}
            </span>
            <span className="h-px w-5 bg-slate-700" />
            <span
              className="font-mono text-[10px] font-semibold tracking-[0.12em]"
              style={{ color }}
            >
              {stage.label}
            </span>
          </div>
          <p className="m-0 mt-2 text-sm font-semibold text-slate-100">
            {stage.title}
          </p>
          <p className="m-0 mt-1 max-w-2xl text-sm leading-6 text-slate-400">
            {stage.description}
          </p>
          <p className="m-0 mt-3 font-mono text-xs leading-5 text-slate-400">
            <span style={{ color }}>Invariant:</span> {stage.invariant}
          </p>
        </div>
        <div className="flex items-center justify-end gap-2 border-t border-slate-800 px-4 py-4 md:border-t-0 md:border-l">
          <button
            type="button"
            onClick={() => goTo(activeIndex - 1)}
            className="flex size-11 items-center justify-center rounded-md border border-slate-700 text-slate-400 hover:text-slate-100 focus-visible:ring-2 focus-visible:ring-sky-400/70 focus-visible:outline-none"
            aria-label="Previous stage"
          >
            <ChevronLeft size={16} />
          </button>
          <button
            type="button"
            onClick={() => goTo(activeIndex + 1)}
            className="flex size-11 items-center justify-center rounded-md border border-slate-700 text-slate-400 hover:text-slate-100 focus-visible:ring-2 focus-visible:ring-sky-400/70 focus-visible:outline-none"
            aria-label="Next stage"
          >
            <ChevronRight size={16} />
          </button>
        </div>
      </div>

      <figcaption className="border-t border-slate-800 bg-[#070b12] px-4 py-3 text-center text-xs leading-5 text-slate-500 sm:px-5">
        {story.caption}
      </figcaption>
    </figure>
  )
}
