"use client"

import {
  ChevronLeft,
  ChevronRight,
  Pause,
  Play,
  RotateCcw,
} from "lucide-react"
import {
  type KeyboardEvent as ReactKeyboardEvent,
  useCallback,
  useEffect,
  useRef,
  useState,
  useSyncExternalStore,
} from "react"

import { cn } from "@/lib/utils"

type StoryName = "overview" | "add" | "push" | "failure" | "hydrate"
type NodeKind = "file" | "git" | "xet" | "control" | "store"
type StepTone = "info" | "warning" | "success" | "danger"

type DiagramNode = {
  id: string
  x: number
  y: number
  width: number
  height: number
  title: string
  detail: string
  kind: NodeKind
  firstStep: number
}

type DiagramEdge = {
  id: string
  path: string
  step: number
  label?: string
  labelX?: number
  labelY?: number
}

type StoryStep = {
  label: string
  title: string
  description: string
  invariant: string
  activeNodes: string[]
  tone?: StepTone
}

type Story = {
  eyebrow: string
  title: string
  caption: string
  nodes: DiagramNode[]
  edges: DiagramEdge[]
  steps: StoryStep[]
}

const STORY_COLORS: Record<NodeKind, string> = {
  file: "#94a3b8",
  git: "#f97316",
  xet: "#22d3ee",
  control: "#a78bfa",
  store: "#38bdf8",
}

const TONE_COLORS: Record<StepTone, string> = {
  info: "#38bdf8",
  warning: "#fbbf24",
  success: "#34d399",
  danger: "#fb7185",
}

const stories: Record<StoryName, Story> = {
  overview: {
    eyebrow: "SYSTEM TRACE 01",
    title: "One history, two physical data paths",
    caption:
      "Git owns names and history. Crab chooses each file's representation, then joins both lanes at one publication boundary.",
    nodes: [
      { id: "worktree", x: 30, y: 174, width: 132, height: 78, title: "Worktree", detail: "real file bytes", kind: "file", firstStep: 0 },
      { id: "rules", x: 200, y: 174, width: 142, height: 78, title: ".gitattributes", detail: "select per path", kind: "control", firstStep: 0 },
      { id: "git", x: 402, y: 64, width: 156, height: 82, title: "Git object graph", detail: "blob · tree · commit", kind: "git", firstStep: 1 },
      { id: "stage", x: 402, y: 286, width: 156, height: 82, title: "Crab staging", detail: "chunks · recipes", kind: "xet", firstStep: 2 },
      { id: "pack", x: 628, y: 64, width: 146, height: 82, title: "Git pack", detail: "ordinary + pointer", kind: "git", firstStep: 3 },
      { id: "xet", x: 628, y: 286, width: 146, height: 82, title: "Xet objects", detail: "xorbs + shards", kind: "xet", firstStep: 3 },
      { id: "ref", x: 816, y: 174, width: 116, height: 78, title: "Ref journal", detail: "visible state", kind: "store", firstStep: 4 },
    ],
    edges: [
      { id: "inspect", path: "M162 213 H200", step: 0, label: "inspect", labelX: 181, labelY: 202 },
      { id: "ordinary", path: "M342 198 C370 198 372 105 402 105", step: 1, label: "ordinary", labelX: 374, labelY: 154 },
      { id: "tracked", path: "M342 228 C370 228 372 327 402 327", step: 2, label: "filter=crab", labelX: 374, labelY: 278 },
      { id: "pack", path: "M558 105 H628", step: 3 },
      { id: "xet", path: "M558 327 H628", step: 3 },
      { id: "publish-git", path: "M774 105 C804 105 796 196 816 202", step: 4 },
      { id: "publish-xet", path: "M774 327 C804 327 796 230 816 224", step: 4 },
    ],
    steps: [
      { label: "SELECT", title: "Classify the path", description: "Crab reads Git's own attributes to decide whether a file stays ordinary or uses the large-file representation.", invariant: "Selection is per path, not per repository.", activeNodes: ["worktree", "rules"] },
      { label: "GIT LANE", title: "Keep ordinary bytes ordinary", description: "Source files remain standard Git blobs and continue through the object graph without Crab metadata.", invariant: "Pure Git pushes can skip the Xet data plane.", activeNodes: ["rules", "git"] },
      { label: "XET LANE", title: "Interpret tracked large content", description: "The commit records a pointer while Crab stages the ordered chunks needed to reconstruct the real file.", invariant: "The worktree keeps the full file; Git stores its identity.", activeNodes: ["rules", "stage"] },
      { label: "DURABLE", title: "Upload both immutable lanes", description: "Git objects become a pack; new chunk content becomes xorbs and shards in cloud object storage.", invariant: "Content-addressed dependencies are written before visibility.", activeNodes: ["git", "stage", "pack", "xet"] },
      { label: "VISIBLE", title: "Publish one repository state", description: "A single expected-old ref transaction makes the commit—and everything it names—reachable.", invariant: "One history. Two data paths. One publication boundary.", activeNodes: ["pack", "xet", "ref"], tone: "success" },
    ],
  },
  add: {
    eyebrow: "COMMAND TRACE 02",
    title: "crab add chooses and prepares the representation",
    caption:
      "The large-file lane streams and flushes its bytes before Git's index receives the pointer. The ordinary lane remains native Git.",
    nodes: [
      { id: "files", x: 26, y: 174, width: 134, height: 78, title: "Input paths", detail: "source + model", kind: "file", firstStep: 0 },
      { id: "match", x: 196, y: 174, width: 142, height: 78, title: "Attribute match", detail: "filter=crab?", kind: "control", firstStep: 0 },
      { id: "blob", x: 394, y: 58, width: 148, height: 82, title: "Git blob", detail: "exact source bytes", kind: "git", firstStep: 1 },
      { id: "stream", x: 378, y: 270, width: 178, height: 98, title: "BLAKE3 + CDC", detail: "bounded stream", kind: "xet", firstStep: 2 },
      { id: "stage", x: 610, y: 270, width: 144, height: 98, title: "Local staging", detail: "ordered chunks", kind: "xet", firstStep: 2 },
      { id: "pointer", x: 610, y: 58, width: 144, height: 82, title: "Pointer blob", detail: "hash · size · hint", kind: "git", firstStep: 3 },
      { id: "index", x: 808, y: 174, width: 126, height: 78, title: "Git index", detail: "ready to commit", kind: "git", firstStep: 4 },
    ],
    edges: [
      { id: "match", path: "M160 213 H196", step: 0 },
      { id: "normal", path: "M338 196 C366 196 366 99 394 99", step: 1, label: "ordinary", labelX: 367, labelY: 149 },
      { id: "stream", path: "M338 230 C360 230 354 319 378 319", step: 2, label: "tracked", labelX: 359, labelY: 278 },
      { id: "chunks", path: "M556 319 H610", step: 2, label: "chunks", labelX: 583, labelY: 307 },
      { id: "flush", path: "M682 270 V140", step: 3, label: "flush", labelX: 702, labelY: 208 },
      { id: "blob-index", path: "M542 99 C740 99 760 195 808 202", step: 4 },
      { id: "pointer-index", path: "M754 99 C790 99 782 190 808 202", step: 4 },
    ],
    steps: [
      { label: "MATCH", title: "Resolve Git attributes", description: "Each path is classified with the same .gitattributes rules that Git uses for its filter process.", invariant: "A tracked pattern does not change unrelated paths.", activeNodes: ["files", "match"] },
      { label: "NORMAL", title: "Write the normal Git blob", description: "Ordinary source bytes are hashed into Git's object database with no chunk metadata or staging work.", invariant: "Git remains the canonical representation for ordinary content.", activeNodes: ["match", "blob"] },
      { label: "STREAM", title: "Hash and chunk in one bounded pass", description: "Crab computes the full-file BLAKE3 identity while content-defined chunking finds reusable regions.", invariant: "The file is never required to fit in memory.", activeNodes: ["match", "stream", "stage"] },
      { label: "FLUSH", title: "Close staging before the pointer", description: "Ordered chunk bytes and their recipe are flushed before Crab writes the small pointer blob.", invariant: "The pointer never outruns its local reconstruction data.", activeNodes: ["stream", "stage", "pointer"], tone: "warning" },
      { label: "INDEX", title: "Present one Git index", description: "The index contains the full ordinary blob and the large file's pointer, ready for one normal Git commit.", invariant: "crab add changes the indexed representation, not worktree bytes.", activeNodes: ["blob", "pointer", "index"], tone: "success" },
    ],
  },
  push: {
    eyebrow: "TRANSACTION TRACE 03",
    title: "Push makes dependencies durable, then moves the ref",
    caption:
      "Git and Xet uploads overlap behind a per-ref lock. Verification gates the only operation that changes repository visibility.",
    nodes: [
      { id: "helper", x: 20, y: 174, width: 126, height: 78, title: "Remote helper", detail: "push batch", kind: "control", firstStep: 0 },
      { id: "discover", x: 178, y: 174, width: 132, height: 78, title: "Discovery", detail: "objects + pointers", kind: "control", firstStep: 0 },
      { id: "lock", x: 178, y: 54, width: 132, height: 70, title: "Ref lock", detail: "serialize writers", kind: "control", firstStep: 1 },
      { id: "gitpack", x: 362, y: 66, width: 138, height: 80, title: "Git pack", detail: "reachable objects", kind: "git", firstStep: 2 },
      { id: "xorbs", x: 362, y: 278, width: 138, height: 94, title: "Xorbs + shards", detail: "new chunks + map", kind: "xet", firstStep: 2 },
      { id: "origin", x: 552, y: 154, width: 144, height: 118, title: "Object store", detail: "immutable origin", kind: "store", firstStep: 3 },
      { id: "verify", x: 744, y: 174, width: 122, height: 78, title: "Closure proof", detail: "all dependencies", kind: "control", firstStep: 4 },
      { id: "ref", x: 744, y: 326, width: 122, height: 70, title: "Ref journal", detail: "expected-old", kind: "store", firstStep: 5 },
      { id: "manifest", x: 552, y: 326, width: 144, height: 70, title: "Manifest", detail: "derived snapshot", kind: "store", firstStep: 6 },
    ],
    edges: [
      { id: "discover", path: "M146 213 H178", step: 0 },
      { id: "lock", path: "M244 174 V124", step: 1 },
      { id: "gitpack", path: "M310 195 C334 195 334 106 362 106", step: 2 },
      { id: "xorbs", path: "M310 231 C334 231 334 325 362 325", step: 2 },
      { id: "upload-git", path: "M500 106 C530 106 526 183 552 190", step: 3 },
      { id: "upload-xet", path: "M500 325 C530 325 526 243 552 236", step: 3 },
      { id: "verify", path: "M696 213 H744", step: 4 },
      { id: "commit", path: "M805 252 V326", step: 5, label: "commit", labelX: 826, labelY: 292 },
      { id: "compact", path: "M744 361 H696", step: 6, label: "fold", labelX: 720, labelY: 350 },
    ],
    steps: [
      { label: "DISCOVER", title: "Discover the complete push closure", description: "The helper walks reachable Git objects and parses Crab pointers before opening any large-file metadata path.", invariant: "No pointers means a pure-Git fast path.", activeNodes: ["helper", "discover"] },
      { label: "LOCK", title: "Serialize writers to the destination ref", description: "The lock is acquired before pack publication and remains owned through the ref decision.", invariant: "Concurrent writers cannot silently replace one another.", activeNodes: ["discover", "lock"] },
      { label: "PACK", title: "Build both immutable representations", description: "Git objects become a pack while new chunks are deduplicated into compressed xorbs and file-mapping shards.", invariant: "The pipelines are bounded, parallel, and backpressured.", activeNodes: ["gitpack", "xorbs"] },
      { label: "UPLOAD", title: "Put immutable objects at the origin", description: "The Git pack, xorbs, and shards upload concurrently because none of them changes what a ref exposes.", invariant: "Retries may leave safe immutable orphans, never partial visible state.", activeNodes: ["gitpack", "xorbs", "origin"] },
      { label: "VERIFY", title: "Prove connectivity and content closure", description: "Crab proves the commit graph is connected and every pointer can resolve through durable shards and xorbs.", invariant: "A cache hint cannot replace canonical origin proof.", activeNodes: ["origin", "verify"], tone: "warning" },
      { label: "REF TXN", title: "Flip repository visibility", description: "The journal commits the new tip only if the destination still matches the expected old object ID.", invariant: "This is the linearization point of the push.", activeNodes: ["lock", "verify", "ref"], tone: "success" },
      { label: "COMPACT", title: "Fold durable history into read state", description: "A compactor produces bounded manifest state after publication; lookup accelerators can be repaired independently.", invariant: "Derived indexes never define whether the ref is valid.", activeNodes: ["ref", "manifest"], tone: "success" },
    ],
  },
  failure: {
    eyebrow: "FAILURE TRACE 04",
    title: "Every interruption has an explicit visible result",
    caption:
      "Step through the failure boundaries. Before the ref transaction, readers keep seeing the old tip; after it, repair never rolls visibility back.",
    nodes: [
      { id: "old", x: 32, y: 170, width: 132, height: 82, title: "Old ref", detail: "still reachable", kind: "git", firstStep: 0 },
      { id: "objects", x: 218, y: 70, width: 148, height: 86, title: "Immutable data", detail: "pack · xorbs · shards", kind: "store", firstStep: 0 },
      { id: "verify", x: 218, y: 274, width: 148, height: 86, title: "Closure check", detail: "reject if incomplete", kind: "control", firstStep: 2 },
      { id: "expected", x: 430, y: 170, width: 148, height: 82, title: "Expected-old", detail: "compare current tip", kind: "control", firstStep: 3 },
      { id: "journal", x: 642, y: 170, width: 136, height: 82, title: "Ref journal", detail: "commit or reject", kind: "store", firstStep: 3 },
      { id: "new", x: 830, y: 70, width: 106, height: 82, title: "New ref", detail: "visible", kind: "git", firstStep: 4 },
      { id: "repair", x: 830, y: 274, width: 106, height: 86, title: "Repair", detail: "rebuild indexes", kind: "xet", firstStep: 4 },
    ],
    edges: [
      { id: "upload", path: "M164 193 C188 193 190 113 218 113", step: 0 },
      { id: "orphan", path: "M292 156 V274", step: 1, label: "safe orphan", labelX: 318, labelY: 220 },
      { id: "reject", path: "M366 317 C404 317 398 232 430 220", step: 2 },
      { id: "compare", path: "M578 211 H642", step: 3 },
      { id: "publish", path: "M778 194 C810 194 804 111 830 111", step: 4 },
      { id: "repair", path: "M710 252 C742 300 792 317 830 317", step: 4 },
    ],
    steps: [
      { label: "PREPARE", title: "Uploads are not publication", description: "Crab may write any number of immutable objects while the old branch tip remains the only reachable state.", invariant: "VISIBLE: old ref", activeNodes: ["old", "objects"] },
      { label: "UPLOAD FAIL", title: "An interrupted upload leaves safe garbage", description: "Content-addressed objects already completed remain valid but unreachable; the grace-period GC can reclaim them later.", invariant: "VISIBLE: old ref · no missing reachable content", activeNodes: ["old", "objects"], tone: "danger" },
      { label: "VERIFY FAIL", title: "Incomplete closure is rejected", description: "If a Git object, shard, or xorb cannot be proven durable, verification stops before the ref journal.", invariant: "VISIBLE: old ref · push rejected", activeNodes: ["old", "objects", "verify"], tone: "danger" },
      { label: "REF CONFLICT", title: "A stale writer loses explicitly", description: "Expected-old comparison detects that another writer moved the destination and refuses the stale update.", invariant: "VISIBLE: winner's ref · stale edit rejected", activeNodes: ["old", "expected", "journal"], tone: "warning" },
      { label: "COMMITTED", title: "Post-commit repair is fail-forward", description: "Once the journal commits, the new ref is valid. Manifest and locator failures rebuild from durable source state.", invariant: "VISIBLE: new ref · accelerators repairable", activeNodes: ["journal", "new", "repair"], tone: "success" },
    ],
  },
  hydrate: {
    eyebrow: "READ TRACE 05",
    title: "A pointer becomes byte-identical worktree content",
    caption:
      "Crab follows metadata to the minimum object-store ranges, reconstructs the ordered chunks, and verifies the full-file identity before materializing it.",
    nodes: [
      { id: "checkout", x: 24, y: 174, width: 126, height: 78, title: "Git checkout", detail: "pointer blob", kind: "git", firstStep: 0 },
      { id: "pointer", x: 184, y: 174, width: 130, height: 78, title: "Pointer", detail: "hash · size · hint", kind: "git", firstStep: 0 },
      { id: "shard", x: 354, y: 64, width: 138, height: 82, title: "Shard lookup", detail: "ordered terms", kind: "xet", firstStep: 1 },
      { id: "ranges", x: 354, y: 284, width: 138, height: 82, title: "Range plan", detail: "coalesce reads", kind: "control", firstStep: 2 },
      { id: "origin", x: 546, y: 174, width: 138, height: 78, title: "Object store", detail: "xorb byte ranges", kind: "store", firstStep: 3 },
      { id: "rebuild", x: 728, y: 64, width: 140, height: 82, title: "Reconstruct", detail: "ordered chunks", kind: "xet", firstStep: 4 },
      { id: "verify", x: 728, y: 284, width: 140, height: 82, title: "BLAKE3 verify", detail: "full-file identity", kind: "control", firstStep: 4 },
      { id: "worktree", x: 892, y: 174, width: 58, height: 78, title: "File", detail: "bytes", kind: "file", firstStep: 5 },
    ],
    edges: [
      { id: "pointer", path: "M150 213 H184", step: 0 },
      { id: "lookup", path: "M314 196 C334 196 332 105 354 105", step: 1 },
      { id: "plan", path: "M423 146 V284", step: 2 },
      { id: "get", path: "M492 325 C522 325 518 230 546 220", step: 3, label: "range GET", labelX: 519, labelY: 279 },
      { id: "rebuild", path: "M684 196 C708 196 704 105 728 105", step: 4 },
      { id: "verify", path: "M798 146 V284", step: 4 },
      { id: "hydrate", path: "M868 325 C886 325 878 233 892 222", step: 5 },
    ],
    steps: [
      { label: "CHECKOUT", title: "Git supplies the pointer", description: "The Git graph stays compact and reveals the content identity without embedding the large file in the pack.", invariant: "Git can inspect history without fetching large bytes.", activeNodes: ["checkout", "pointer"] },
      { label: "RESOLVE", title: "Resolve every reconstruction term", description: "Shard metadata maps the file recipe to the xorb ranges that contain each ordered chunk.", invariant: "Terms must cover every chunk in the file version.", activeNodes: ["pointer", "shard"] },
      { label: "PLAN", title: "Turn chunk terms into efficient reads", description: "Adjacent ranges are coalesced so object-store request overhead does not grow one-for-one with chunk count.", invariant: "Metadata preserves order while the planner reduces fan-out.", activeNodes: ["shard", "ranges"] },
      { label: "RANGE GET", title: "Fetch only the ranges the file needs", description: "Readers stream xorb ranges from the origin or a verified cache instead of downloading repository-sized packfiles.", invariant: "Read work scales with requested content.", activeNodes: ["ranges", "origin"] },
      { label: "VERIFY", title: "Reconstruct and prove identity", description: "Chunks are emitted in recipe order and the full output is checked against the pointer's BLAKE3 hash.", invariant: "Output is byte-identical or hydration returns an error.", activeNodes: ["origin", "rebuild", "verify"], tone: "warning" },
      { label: "HYDRATE", title: "Materialize normal worktree bytes", description: "The verified file replaces the pointer representation at the worktree boundary for tools and developers.", invariant: "The application sees a normal file, not a storage protocol.", activeNodes: ["verify", "worktree"], tone: "success" },
    ],
  },
}

const AUTO_ADVANCE_MS = 3_800

function subscribeToReducedMotion(update: () => void) {
  const media = window.matchMedia("(prefers-reduced-motion: reduce)")
  media.addEventListener("change", update)
  return () => media.removeEventListener("change", update)
}

function usePrefersReducedMotion() {
  return useSyncExternalStore(
    subscribeToReducedMotion,
    () => window.matchMedia("(prefers-reduced-motion: reduce)").matches,
    () => true,
  )
}

function nodeState(node: DiagramNode, step: StoryStep, activeIndex: number) {
  if (step.activeNodes.includes(node.id)) return "active"
  if (node.firstStep < activeIndex) return "complete"
  return "future"
}

function NodeGlyph({ kind, color }: { kind: NodeKind; color: string }) {
  if (kind === "store") {
    return (
      <g fill="none" stroke={color} strokeWidth="1.5">
        <ellipse cx="0" cy="-6" rx="9" ry="3" />
        <path d="M-9-6v12c0 1.7 4 3 9 3s9-1.3 9-3V-6M-9 0c0 1.7 4 3 9 3s9-1.3 9-3" />
      </g>
    )
  }

  if (kind === "file") {
    return <path d="M-7-10h9l6 6v14H-7zM2-10v6h6M-3 2h7M-3 6h7" fill="none" stroke={color} strokeWidth="1.5" />
  }

  if (kind === "git") {
    return (
      <g fill="none" stroke={color} strokeWidth="1.5">
        <circle cx="-6" cy="-6" r="3" /><circle cx="6" cy="6" r="3" /><circle cx="-6" cy="7" r="3" />
        <path d="M-6-3v7M-3-6h3c4 0 6 3 6 9" />
      </g>
    )
  }

  if (kind === "xet") {
    return (
      <g fill="none" stroke={color} strokeWidth="1.5">
        <rect x="-10" y="-9" width="8" height="8" /><rect x="2" y="-9" width="8" height="8" />
        <rect x="-10" y="3" width="8" height="8" /><rect x="2" y="3" width="8" height="8" />
      </g>
    )
  }

  return <path d="M0-10v5M0 5v5M-10 0h5M5 0h5M-6-6l3 3M3 3l3 3M6-6L3-3M-3 3l-3 3" fill="none" stroke={color} strokeWidth="1.5" />
}

export function GitArchitecturePlayer({ story: storyName }: { story: StoryName }) {
  const story = stories[storyName]
  const figureRef = useRef<HTMLElement>(null)
  const stageScrollerRef = useRef<HTMLDivElement>(null)
  const tabRefs = useRef<Array<HTMLButtonElement | null>>([])
  const [activeIndex, setActiveIndex] = useState(0)
  const [paused, setPaused] = useState(true)
  const [inView, setInView] = useState(false)
  const [hasInteracted, setHasInteracted] = useState(false)
  const reducedMotion = usePrefersReducedMotion()
  const step = story.steps[activeIndex]
  const tone = step.tone ?? "info"
  const activeColor = TONE_COLORS[tone]
  const controlsPaused = paused || reducedMotion
  const playbackPaused = controlsPaused || !inView
  const panelId = `${storyName}-stage-panel`

  useEffect(() => {
    const figure = figureRef.current
    if (!figure) return

    const observer = new IntersectionObserver(
      ([entry]) => setInView(entry.isIntersecting),
      { threshold: 0.15 },
    )
    observer.observe(figure)
    return () => observer.disconnect()
  }, [])

  useEffect(() => {
    if (playbackPaused) return
    const timer = window.setTimeout(() => {
      setActiveIndex((current) => (current + 1) % story.steps.length)
    }, AUTO_ADVANCE_MS)
    return () => window.clearTimeout(timer)
  }, [activeIndex, playbackPaused, story.steps.length])

  const goTo = useCallback(
    (index: number) => {
      setHasInteracted(true)
      setActiveIndex((index + story.steps.length) % story.steps.length)
    },
    [story.steps.length],
  )

  useEffect(() => {
    const scroller = stageScrollerRef.current
    const tab = tabRefs.current[activeIndex]
    if (!scroller || !tab) return

    const tabStart = tab.offsetLeft
    const tabEnd = tabStart + tab.offsetWidth
    const visibleStart = scroller.scrollLeft
    const visibleEnd = visibleStart + scroller.clientWidth

    if (tabStart < visibleStart) scroller.scrollLeft = tabStart
    if (tabEnd > visibleEnd) scroller.scrollLeft = tabEnd - scroller.clientWidth
  }, [activeIndex])

  const moveTabFocus = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    let nextIndex: number | undefined
    if (event.key === "ArrowRight") nextIndex = activeIndex + 1
    if (event.key === "ArrowLeft") nextIndex = activeIndex - 1
    if (event.key === "Home") nextIndex = 0
    if (event.key === "End") nextIndex = story.steps.length - 1
    if (nextIndex === undefined) return

    event.preventDefault()
    const normalizedIndex = (nextIndex + story.steps.length) % story.steps.length
    goTo(normalizedIndex)
    tabRefs.current[normalizedIndex]?.focus()
  }

  return (
    <figure ref={figureRef} className="my-10 overflow-hidden rounded-xl border border-slate-800 bg-[#070b12] shadow-[0_24px_80px_rgba(2,6,23,0.24)]">
      <div className="border-b border-slate-800/90 bg-[#0b111a] px-4 py-4 sm:px-5">
        <div className="flex flex-wrap items-end justify-between gap-3">
          <div>
            <p className="m-0 font-mono text-[10px] font-semibold tracking-[0.22em] text-sky-400">{story.eyebrow}</p>
            <h3 className="m-0 mt-1 text-base font-semibold tracking-tight text-slate-100 sm:text-lg">{story.title}</h3>
          </div>
          <div className="font-mono text-[10px] tracking-[0.12em] text-slate-600" aria-hidden="true">
            INTERACTIVE TRACE
          </div>
        </div>

        <div ref={stageScrollerRef} className="-mx-1 mt-4 overflow-x-auto px-1 pb-1">
          <div
            className="flex min-w-max items-center gap-1.5"
            role="tablist"
            aria-label={`${story.title} stages`}
            onKeyDown={moveTabFocus}
          >
            {story.steps.map((candidate, index) => {
              const isActive = index === activeIndex
              const isComplete = index < activeIndex
              return (
                <button
                  key={candidate.label}
                  ref={(element) => {
                    tabRefs.current[index] = element
                  }}
                  type="button"
                  role="tab"
                  id={`${storyName}-stage-${index}`}
                  aria-controls={panelId}
                  aria-selected={isActive}
                  tabIndex={isActive ? 0 : -1}
                  onClick={() => goTo(index)}
                  className={cn(
                    "group flex min-h-11 items-center gap-2 rounded-md border px-3 font-mono text-xs font-semibold tracking-[0.08em] transition-colors duration-150 focus-visible:ring-2 focus-visible:ring-sky-400/70 focus-visible:ring-offset-2 focus-visible:ring-offset-[#0b111a] focus-visible:outline-none",
                    isActive
                      ? "border-sky-400/60 bg-sky-400/10 text-sky-300"
                      : isComplete
                        ? "border-emerald-400/20 bg-emerald-400/5 text-emerald-300/80"
                        : "border-slate-800 bg-slate-950/40 text-slate-500 hover:border-slate-700 hover:text-slate-300",
                  )}
                >
                  <span
                    className={cn(
                      "flex size-5 items-center justify-center rounded-full border text-[9px]",
                      isActive ? "border-sky-400/70" : isComplete ? "border-emerald-400/40" : "border-slate-700",
                    )}
                  >
                    {index + 1}
                  </span>
                  {candidate.label}
                </button>
              )
            })}
          </div>
        </div>
      </div>

      <div className="overflow-x-auto" aria-hidden="true">
        <svg
          className="block w-full min-w-[700px]"
          viewBox="0 0 960 430"
          focusable="false"
        >
          <defs>
            {[
              { id: "active", color: activeColor, opacity: 0.9 },
              { id: "complete", color: "#64748b", opacity: 0.55 },
              { id: "future", color: "#334155", opacity: 0.42 },
            ].map((marker) => (
              <marker
                key={marker.id}
                id={`arrow-${storyName}-${marker.id}`}
                viewBox="0 0 9 9"
                refX="9"
                refY="4.5"
                markerWidth="9"
                markerHeight="9"
                markerUnits="userSpaceOnUse"
                orient="auto"
              >
                <path d="M0 0 9 4.5 0 9Z" fill={marker.color} fillOpacity={marker.opacity} />
              </marker>
            ))}
          </defs>
          <rect width="960" height="430" fill="#070b12" />
          <text x="28" y="32" fill="#475569" fontFamily="ui-monospace, SFMono-Regular, Menlo, monospace" fontSize="11.5" letterSpacing="1.4">
            IMMUTABLE DATA
          </text>
          <line x1="132" y1="28" x2="410" y2="28" stroke="#1e293b" />
          <text x="756" y="32" fill="#475569" fontFamily="ui-monospace, SFMono-Regular, Menlo, monospace" fontSize="11.5" letterSpacing="1.4">
            VISIBLE STATE
          </text>
          <line x1="850" y1="28" x2="932" y2="28" stroke="#1e293b" />

          {story.edges.map((edge) => {
            const active = edge.step === activeIndex
            const complete = edge.step < activeIndex
            const visible = edge.step <= activeIndex
            const stroke = active ? activeColor : complete ? "#64748b" : "#334155"
            const arrowState = active ? "active" : complete ? "complete" : "future"
            return (
              <g key={edge.id} opacity={visible ? 1 : 0.28} style={{ transition: reducedMotion ? "none" : "opacity 350ms ease" }}>
                <path
                  d={edge.path}
                  fill="none"
                  stroke={stroke}
                  strokeOpacity={active ? 0.9 : complete ? 0.55 : 0.42}
                  strokeWidth={active ? 2 : 1.25}
                  strokeDasharray={active ? "5 6" : undefined}
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  markerEnd={`url(#arrow-${storyName}-${arrowState})`}
                  vectorEffect="non-scaling-stroke"
                />
                {edge.label && (
                  <text
                    x={edge.labelX}
                    y={edge.labelY}
                    fill={active ? activeColor : "#64748b"}
                    fontFamily="ui-monospace, SFMono-Regular, Menlo, monospace"
                    fontSize="10.5"
                    textAnchor="middle"
                  >
                    {edge.label.toUpperCase()}
                  </text>
                )}
              </g>
            )
          })}

          {story.nodes.map((node) => {
            const state = nodeState(node, step, activeIndex)
            const active = state === "active"
            const complete = state === "complete"
            const color = active ? activeColor : complete ? "#64748b" : STORY_COLORS[node.kind]
            return (
              <g
                key={node.id}
                transform={`translate(${node.x} ${node.y})`}
                opacity={active ? 1 : complete ? 0.78 : 0.38}
                style={{ transition: reducedMotion ? "none" : "opacity 350ms ease" }}
              >
                <rect
                  width={node.width}
                  height={node.height}
                  rx="9"
                  fill={active ? `${activeColor}12` : "#0b111a"}
                  stroke={color}
                  strokeOpacity={active ? 0.9 : complete ? 0.45 : 0.5}
                  strokeWidth={active ? 1.75 : 1}
                  vectorEffect="non-scaling-stroke"
                />
                <g transform="translate(22 29)">
                  <NodeGlyph kind={node.kind} color={color} />
                </g>
                <text x="42" y="27" fill={active ? "#f8fafc" : "#cbd5e1"} fontFamily="Inter, ui-sans-serif, system-ui" fontSize="14" fontWeight="600">
                  {node.title}
                </text>
                <text x="42" y="46" fill={active ? "#94a3b8" : "#64748b"} fontFamily="ui-monospace, SFMono-Regular, Menlo, monospace" fontSize="10">
                  {node.detail}
                </text>
                <circle cx={node.width - 14} cy="14" r="3" fill={color} opacity={active ? 1 : 0.55} />
              </g>
            )
          })}
        </svg>
      </div>

      <div className="grid border-t border-slate-800/90 bg-[#0b111a] md:grid-cols-[1fr_auto]">
        <div
          id={panelId}
          role="tabpanel"
          aria-labelledby={`${storyName}-stage-${activeIndex}`}
          className="min-h-40 min-w-0 px-4 py-5 focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-sky-400/70 focus-visible:outline-none sm:px-5"
          aria-live={hasInteracted ? "polite" : "off"}
          tabIndex={0}
        >
          <div className="flex items-center gap-2">
            <span className="font-mono text-[10px] text-slate-500">{String(activeIndex + 1).padStart(2, "0")}</span>
            <span className="h-px w-5 bg-slate-700" />
            <span className="font-mono text-[10px] font-semibold tracking-[0.12em]" style={{ color: activeColor }}>{step.label}</span>
          </div>
          <p className="m-0 mt-2 text-sm font-semibold text-slate-100">{step.title}</p>
          <p className="m-0 mt-1 max-w-2xl text-sm leading-6 text-slate-400">{step.description}</p>
          <div className="mt-3 flex items-start gap-2 font-mono text-sm leading-5 text-slate-400">
            <span className="mt-1.5 size-1.5 shrink-0 rounded-full" style={{ backgroundColor: activeColor }} />
            <span>{step.invariant}</span>
          </div>
        </div>

        <div className="flex items-center justify-between gap-2 border-t border-slate-800 px-4 py-4 md:min-w-64 md:justify-end md:border-t-0 md:border-l">
          <button type="button" onClick={() => goTo(activeIndex - 1)} className="flex size-11 items-center justify-center rounded-md border border-slate-700 text-slate-400 transition-colors duration-150 hover:border-slate-600 hover:text-slate-100 focus-visible:ring-2 focus-visible:ring-sky-400/70 focus-visible:ring-offset-2 focus-visible:ring-offset-[#0b111a] focus-visible:outline-none" aria-label="Previous stage">
            <ChevronLeft size={16} />
          </button>
          <button
            type="button"
            onClick={() => {
              setHasInteracted(true)
              setPaused((current) => !current)
            }}
            disabled={reducedMotion}
            className="flex h-11 min-w-24 items-center justify-center gap-2 rounded-md border border-sky-400/40 bg-sky-400/10 px-3 font-mono text-xs font-semibold tracking-wide text-sky-300 transition-colors duration-150 hover:bg-sky-400/15 focus-visible:ring-2 focus-visible:ring-sky-400/70 focus-visible:ring-offset-2 focus-visible:ring-offset-[#0b111a] focus-visible:outline-none disabled:cursor-not-allowed disabled:border-slate-700 disabled:bg-slate-900 disabled:text-slate-500"
            aria-label={controlsPaused ? "Play automatic stage progression" : "Pause automatic stage progression"}
          >
            {controlsPaused ? <Play size={13} /> : <Pause size={13} />}
            {reducedMotion ? "MANUAL" : controlsPaused ? "PLAY" : "PAUSE"}
          </button>
          <button type="button" onClick={() => goTo(activeIndex + 1)} className="flex size-11 items-center justify-center rounded-md border border-slate-700 text-slate-400 transition-colors duration-150 hover:border-slate-600 hover:text-slate-100 focus-visible:ring-2 focus-visible:ring-sky-400/70 focus-visible:ring-offset-2 focus-visible:ring-offset-[#0b111a] focus-visible:outline-none" aria-label="Next stage">
            <ChevronRight size={16} />
          </button>
          <button type="button" onClick={() => goTo(0)} className="flex size-11 items-center justify-center rounded-md border border-slate-800 text-slate-500 transition-colors duration-150 hover:border-slate-700 hover:text-slate-300 focus-visible:ring-2 focus-visible:ring-sky-400/70 focus-visible:ring-offset-2 focus-visible:ring-offset-[#0b111a] focus-visible:outline-none" aria-label="Restart trace">
            <RotateCcw size={14} />
          </button>
        </div>
      </div>

      <figcaption className="border-t border-slate-800/80 bg-[#070b12] px-4 py-3 text-center text-xs leading-5 text-slate-500 sm:px-5">
        <span className="sm:hidden">Swipe the canvas to inspect the full trace. </span>
        {story.caption}
      </figcaption>
    </figure>
  )
}
