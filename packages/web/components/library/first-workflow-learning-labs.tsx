"use client"

import {
  ArchiveRestore,
  ArrowRight,
  Boxes,
  Check,
  Cloud,
  Code2,
  Database,
  FlaskConical,
  FolderInput,
  GitBranch,
  GitCommit,
  HardDrive,
  LockKeyhole,
  Network,
  PackageCheck,
  Route,
  ShieldCheck,
  Sparkles,
  Terminal,
  UnlockKeyhole,
  Users,
  Workflow,
  type LucideIcon,
} from "lucide-react"
import Link from "next/link"
import { useId, useState } from "react"

import { cn } from "@/lib/utils"

type DailySurface = {
  label: string
  value: string
  detail: string
  icon: LucideIcon
  tone: "git" | "local" | "remote" | "team"
}

type DailyStage = {
  id: "sync" | "select" | "lock" | "stage" | "publish" | "release"
  label: string
  verb: string
  command: string
  title: string
  summary: string
  truth: string
  icon: LucideIcon
  surfaces: DailySurface[]
}

const DAILY_STAGES: DailyStage[] = [
  {
    id: "sync",
    label: "1 · Sync",
    verb: "Start clean",
    command: "crab pull --no-hydrate",
    title: "Update identity before downloading bytes",
    summary:
      "Git receives the current commit and Crab leaves managed files as pointers until you choose a working set.",
    truth: "The branch is current; large bytes have not crossed the network.",
    icon: GitBranch,
    surfaces: [
      {
        label: "Git history",
        value: "current branch tip",
        detail: "commits + pointers",
        icon: GitCommit,
        tone: "git",
      },
      {
        label: "Working set",
        value: "pointer-first",
        detail: "source stays available",
        icon: HardDrive,
        tone: "local",
      },
      {
        label: "Shared bucket",
        value: "readable",
        detail: "no payload fetched",
        icon: Cloud,
        tone: "remote",
      },
      {
        label: "Team signal",
        value: "no edit claimed",
        detail: "safe to inspect",
        icon: Users,
        tone: "team",
      },
    ],
  },
  {
    id: "select",
    label: "2 · Select",
    verb: "Choose bytes",
    command: "crab hydrate --manifest .crab/manifests/evaluation.txt",
    title: "Materialize only what this task needs",
    summary:
      "Crab resolves the selected pointers, reconstructs each file, and verifies its identity before replacement.",
    truth: "The commit is unchanged; selected files now contain normal bytes.",
    icon: FolderInput,
    surfaces: [
      {
        label: "Git history",
        value: "unchanged",
        detail: "same pointer identities",
        icon: GitCommit,
        tone: "git",
      },
      {
        label: "Working set",
        value: "evaluation set",
        detail: "verified bytes local",
        icon: PackageCheck,
        tone: "local",
      },
      {
        label: "Shared bucket",
        value: "range reads",
        detail: "missing chunks only",
        icon: Cloud,
        tone: "remote",
      },
      {
        label: "Team signal",
        value: "inputs declared",
        detail: "manifest is reviewable",
        icon: Users,
        tone: "team",
      },
    ],
  },
  {
    id: "lock",
    label: "3 · Lock",
    verb: "Claim the edit",
    command: "crab lock models/current/encoder.safetensors",
    title: "Signal binary ownership before editing",
    summary:
      "The remote lock tells collaborators who owns a non-mergeable edit before two branches produce conflicting binaries.",
    truth:
      "The file remains editable locally; Crab now coordinates its publisher.",
    icon: LockKeyhole,
    surfaces: [
      {
        label: "Git history",
        value: "unchanged",
        detail: "no commit yet",
        icon: GitCommit,
        tone: "git",
      },
      {
        label: "Working set",
        value: "ready to edit",
        detail: "full bytes local",
        icon: HardDrive,
        tone: "local",
      },
      {
        label: "Shared bucket",
        value: "lock recorded",
        detail: "atomic ownership",
        icon: Cloud,
        tone: "remote",
      },
      {
        label: "Team signal",
        value: "you own the edit",
        detail: "conflict visible early",
        icon: LockKeyhole,
        tone: "team",
      },
    ],
  },
  {
    id: "stage",
    label: "4 · Stage",
    verb: "Review identity",
    command: "crab add models/current/encoder.safetensors",
    title: "Prepare bytes locally and give Git a pointer",
    summary:
      "Crab chunks the edited file and stages its recipe. Git receives the compact pointer that the next commit will record.",
    truth:
      "The pointer and every local reconstruction term agree before commit.",
    icon: Boxes,
    surfaces: [
      {
        label: "Git history",
        value: "pointer staged",
        detail: "hash + size",
        icon: GitCommit,
        tone: "git",
      },
      {
        label: "Working set",
        value: "edited bytes",
        detail: "full file remains",
        icon: HardDrive,
        tone: "local",
      },
      {
        label: "Shared bucket",
        value: "unchanged",
        detail: "nothing published",
        icon: Cloud,
        tone: "remote",
      },
      {
        label: "Team signal",
        value: "lock still held",
        detail: "publisher reserved",
        icon: LockKeyhole,
        tone: "team",
      },
    ],
  },
  {
    id: "publish",
    label: "5 · Publish",
    verb: "Make it durable",
    command: "git commit -m 'Update encoder' && crab push",
    title: "Upload the closure before moving the branch",
    summary:
      "Crab uploads missing Git and large-file dependencies, verifies closure, then advances the destination ref.",
    truth: "Another clean client can now discover and reconstruct the commit.",
    icon: Cloud,
    surfaces: [
      {
        label: "Git history",
        value: "new commit visible",
        detail: "ref advanced",
        icon: GitCommit,
        tone: "git",
      },
      {
        label: "Working set",
        value: "full bytes local",
        detail: "ready for more work",
        icon: HardDrive,
        tone: "local",
      },
      {
        label: "Shared bucket",
        value: "closure durable",
        detail: "xorbs + shards + Git",
        icon: ShieldCheck,
        tone: "remote",
      },
      {
        label: "Team signal",
        value: "change published",
        detail: "lock still explicit",
        icon: Users,
        tone: "team",
      },
    ],
  },
  {
    id: "release",
    label: "6 · Release",
    verb: "Leave it tidy",
    command: "crab unlock models/current/encoder.safetensors",
    title: "Release ownership and choose local retention",
    summary:
      "Unlock the binary after publication. Dehydrate clean files when the next task does not need their local bytes.",
    truth:
      "Shared history stays durable while each client controls its own disk use.",
    icon: UnlockKeyhole,
    surfaces: [
      {
        label: "Git history",
        value: "published",
        detail: "one shared version",
        icon: GitCommit,
        tone: "git",
      },
      {
        label: "Working set",
        value: "keep or dehydrate",
        detail: "local policy",
        icon: ArchiveRestore,
        tone: "local",
      },
      {
        label: "Shared bucket",
        value: "content retained",
        detail: "dehydrate is local",
        icon: Cloud,
        tone: "remote",
      },
      {
        label: "Team signal",
        value: "edit available",
        detail: "lock released",
        icon: UnlockKeyhole,
        tone: "team",
      },
    ],
  },
]

const DAILY_TONES = {
  git: {
    border: "border-[#f28c52]/45",
    background: "bg-[#f28c52]/10",
    icon: "text-[#f5a06c]",
    eyebrow: "text-[#eab18f]",
  },
  local: {
    border: "border-[#4cc9d8]/45",
    background: "bg-[#4cc9d8]/10",
    icon: "text-[#62d7e4]",
    eyebrow: "text-[#9ce6ec]",
  },
  remote: {
    border: "border-[#5e87d8]/45",
    background: "bg-[#5e87d8]/10",
    icon: "text-[#86a9ef]",
    eyebrow: "text-[#b7caf5]",
  },
  team: {
    border: "border-[#9b87f5]/45",
    background: "bg-[#9b87f5]/10",
    icon: "text-[#b4a5fb]",
    eyebrow: "text-[#d2c9ff]",
  },
} as const

/** Shows how Git, local bytes, remote data, and team coordination change during daily work. */
export function DailyWorkflowStateLab() {
  const [stageId, setStageId] = useState<DailyStage["id"]>("sync")
  const panelId = useId()
  const stage =
    DAILY_STAGES.find((candidate) => candidate.id === stageId) ??
    DAILY_STAGES[0]
  const StageIcon = stage.icon

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(68rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.5rem] border border-[#273b52] bg-[#0b1726] text-[#edf6ff] shadow-[0_28px_80px_rgba(7,18,31,0.28)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(68rem,calc(100vw-2rem))] lg:w-[min(68rem,calc(100vw-24.5rem))]">
      <header className="border-b border-[#273b52] px-4 py-5 sm:px-6">
        <div className="flex flex-col gap-3 lg:flex-row lg:items-end lg:justify-between">
          <div>
            <p className="m-0 font-mono text-[10px] font-black tracking-[0.18em] text-[#62d7e4]">
              DAILY STATE LAB / SELECT A MOMENT
            </p>
            <h3 className="m-0 mt-1 text-[1.75rem] leading-tight font-black tracking-[-0.04em] sm:text-[2.15rem]">
              See what changes. Keep what does not.
            </h3>
          </div>
          <p className="m-0 max-w-md text-sm leading-6 text-[#9fb0c5]">
            Every command changes one boundary. Select a moment to see the
            resulting Git, workspace, bucket, and team state.
          </p>
        </div>
      </header>

      <div
        className="grid grid-cols-2 border-b border-[#273b52] bg-[#08121f] sm:grid-cols-3 lg:grid-cols-6"
        role="group"
        aria-label="Daily Crab workflow moments"
      >
        {DAILY_STAGES.map((candidate) => (
          <button
            key={candidate.id}
            type="button"
            aria-pressed={stage.id === candidate.id}
            aria-controls={panelId}
            onClick={() => setStageId(candidate.id)}
            className={cn(
              "relative min-h-12 border-r border-b border-[#273b52] px-3 py-3 text-left font-mono text-[10px] font-black tracking-[0.05em] text-[#8093aa] transition-colors outline-none after:absolute after:inset-x-3 after:bottom-0 after:h-0.5 after:origin-left after:scale-x-0 after:bg-[#4cc9d8] after:transition-transform focus-visible:ring-2 focus-visible:ring-[#62d7e4] focus-visible:ring-inset motion-reduce:transition-none motion-reduce:after:transition-none lg:border-b-0",
              stage.id === candidate.id
                ? "bg-[#122239] text-white after:scale-x-100"
                : "hover:bg-[#0f1d30] hover:text-[#dce9f5]"
            )}
          >
            {candidate.label}
          </button>
        ))}
      </div>

      <div
        id={panelId}
        className="grid lg:grid-cols-[19rem_minmax(0,1fr)]"
        aria-live="polite"
      >
        <section className="border-b border-[#273b52] bg-[#0f1d30] p-4 sm:p-6 lg:border-r lg:border-b-0">
          <div className="flex items-center gap-3">
            <span className="flex size-11 items-center justify-center rounded-xl border border-[#4cc9d8]/35 bg-[#4cc9d8]/10 text-[#62d7e4]">
              <StageIcon className="size-5" aria-hidden="true" />
            </span>
            <div>
              <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#7f93aa]">
                {stage.verb.toUpperCase()}
              </p>
              <p className="m-0 mt-0.5 text-sm font-black text-white">
                {stage.title}
              </p>
            </div>
          </div>
          <div className="mt-5 rounded-xl border border-[#273b52] bg-[#08121f] p-3">
            <div className="flex items-center gap-2 font-mono text-[9px] font-black tracking-[0.14em] text-[#7f93aa]">
              <Terminal
                className="size-3.5 text-[#f5a06c]"
                aria-hidden="true"
              />
              COMMAND
            </div>
            <code className="mt-2 block overflow-x-auto font-mono text-[11px] leading-5 whitespace-pre-wrap text-[#f6c2a3]">
              $ {stage.command}
            </code>
          </div>
          <p className="m-0 mt-4 text-sm leading-6 text-[#aebed0]">
            {stage.summary}
          </p>
        </section>

        <section className="p-4 sm:p-6">
          <div className="mb-3 flex items-center gap-2 font-mono text-[9px] font-black tracking-[0.16em] text-[#7f93aa]">
            <Route className="size-3.5 text-[#86a9ef]" aria-hidden="true" />
            REPOSITORY STATE AFTER THIS COMMAND
          </div>
          <div className="grid gap-3 sm:grid-cols-2">
            {stage.surfaces.map((surface) => {
              const tone = DAILY_TONES[surface.tone]
              const SurfaceIcon = surface.icon
              return (
                <div
                  key={surface.label}
                  className={cn(
                    "rounded-xl border p-4 transition-colors motion-reduce:transition-none",
                    tone.border,
                    tone.background
                  )}
                >
                  <div className="flex items-center justify-between gap-3">
                    <p
                      className={cn(
                        "m-0 font-mono text-[9px] font-black tracking-[0.14em]",
                        tone.eyebrow
                      )}
                    >
                      {surface.label.toUpperCase()}
                    </p>
                    <SurfaceIcon
                      className={cn("size-4", tone.icon)}
                      aria-hidden="true"
                    />
                  </div>
                  <p className="m-0 mt-4 text-base font-black text-white">
                    {surface.value}
                  </p>
                  <p className="m-0 mt-1 text-xs leading-5 text-[#9fb0c5]">
                    {surface.detail}
                  </p>
                </div>
              )
            })}
          </div>
          <div className="mt-4 flex gap-3 rounded-xl border border-[#65c79a]/35 bg-[#65c79a]/10 p-4 text-[#dff9eb]">
            <Check
              className="mt-0.5 size-5 shrink-0 text-[#73d7a9]"
              aria-hidden="true"
            />
            <div>
              <p className="m-0 font-mono text-[9px] font-black tracking-[0.14em] text-[#8fe4bb]">
                NOW TRUE
              </p>
              <p className="m-0 mt-1 text-sm leading-6 font-bold">
                {stage.truth}
              </p>
            </div>
          </div>
        </section>
      </div>
      <figcaption className="border-t border-[#273b52] px-4 py-3 font-mono text-[10px] text-[#7f93aa] sm:px-6">
        Orange is Git identity. Cyan is local materialization. Blue is durable
        object storage. Violet is team coordination.
      </figcaption>
    </figure>
  )
}

type Capability = {
  id:
    | "large-files"
    | "working-set"
    | "mount"
    | "forge"
    | "lfs"
    | "workflow"
    | "experiments"
    | "operations"
  need: string
  label: string
  title: string
  answer: string
  command: string
  boundary: string
  result: string
  href: string
  linkLabel: string
  icon: LucideIcon
  activeNodes: ("git" | "crab" | "bucket" | "workspace")[]
}

const CAPABILITIES: Capability[] = [
  {
    id: "large-files",
    need: "Version large files",
    label: "Native tracking",
    title: "Keep identity in Git and bytes in your bucket",
    answer:
      "Track large paths with Crab-native pointers and reuse unchanged chunks across versions.",
    command: "crab track '*.safetensors'",
    boundary: "File representation and durable large-file storage",
    result: "One Git commit names code and the exact large-file version.",
    href: "/library/xet-protocol-deduplication",
    linkLabel: "See how chunk reuse works",
    icon: Boxes,
    activeNodes: ["git", "crab", "bucket"],
  },
  {
    id: "working-set",
    need: "Keep checkout small",
    label: "Selective hydration",
    title: "Declare the bytes each job needs",
    answer:
      "Clone pointers first, then hydrate one path, pattern, profile, or committed manifest.",
    command: "crab hydrate --manifest .crab/manifests/test.txt",
    boundary: "Local materialization policy",
    result: "Every client shares the commit but controls its own disk use.",
    href: "/library/lazy-checkout-fuse",
    linkLabel: "Compare materialization modes",
    icon: FolderInput,
    activeNodes: ["crab", "bucket", "workspace"],
  },
  {
    id: "mount",
    need: "Browse beyond local disk",
    label: "Snapshot mount",
    title: "Read files and ranges when the app asks",
    answer:
      "Mount a stable repository snapshot when access is sparse and hard to predict.",
    command: "crab mount -r crab://bucket/repo -m /mnt/repo --read-only",
    boundary: "On-demand file and range access",
    result:
      "The application sees a filesystem while Crab controls cache growth.",
    href: "/library/crab-mount-end-to-end",
    linkLabel: "Operate a mount safely",
    icon: HardDrive,
    activeNodes: ["crab", "bucket", "workspace"],
  },
  {
    id: "forge",
    need: "Keep forge reviews",
    label: "Mirror mode",
    title: "Keep GitHub or GitLab as the review plane",
    answer:
      "Publish Crab data first, then send Git refs and pointer blobs through the existing forge workflow.",
    command: "crab init --mirror=origin crab://bucket/repo",
    boundary: "Coordination between the forge and Crab remote",
    result:
      "Pull requests stay on the forge without putting large bytes in normal Git blobs.",
    href: "/docs/cli/getting-started/mirror-mode",
    linkLabel: "Configure mirror mode",
    icon: GitBranch,
    activeNodes: ["git", "crab", "bucket"],
  },
  {
    id: "lfs",
    need: "Preserve LFS history",
    label: "Crab LFS",
    title: "Keep standard LFS pointers without a gateway",
    answer:
      "Use Crab's local transfer agent when existing history or tools require the Git LFS contract.",
    command: "crab lfs install --local",
    boundary: "Git LFS compatibility and direct object transfer",
    result:
      "Existing LFS pointer identity remains readable by compatible tooling.",
    href: "/library/crab-lfs-direct-storage",
    linkLabel: "Open the direct-storage lab",
    icon: Database,
    activeNodes: ["git", "crab", "bucket"],
  },
  {
    id: "workflow",
    need: "Reuse pipeline outputs",
    label: "Workflow cache",
    title: "Bind reusable output to declared inputs",
    answer:
      "Declare stages, dependencies, parameters, and outputs so a matching identity can replay verified results.",
    command: "crab run train",
    boundary: "Pipeline identity, execution, and cache reuse",
    result: "The repository explains why a stage ran or reused prior output.",
    href: "/docs/cli/workflow/quickstart",
    linkLabel: "Run the workflow quickstart",
    icon: Workflow,
    activeNodes: ["git", "crab", "bucket", "workspace"],
  },
  {
    id: "experiments",
    need: "Compare experiments",
    label: "Experiment tracking",
    title: "Run parameter changes in isolated worktrees",
    answer:
      "Queue experiments, compare parameters and metrics, then retain only the results worth sharing.",
    command: "crab exp run --set train.lr=0.002",
    boundary: "Parameterized execution and result comparison",
    result:
      "Each result stays connected to its base commit, inputs, and metrics.",
    href: "/docs/cli/workflow/experiments",
    linkLabel: "Explore experiment workflows",
    icon: FlaskConical,
    activeNodes: ["git", "crab", "workspace"],
  },
  {
    id: "operations",
    need: "Prove repository health",
    label: "Operations",
    title: "Inspect before you repair or delete",
    answer:
      "Use health, integrity, audit, recovery, and storage tools according to the failing boundary.",
    command: "crab doctor && crab fsck",
    boundary: "Integrity, recovery, retention, and cost",
    result:
      "Operators act on explicit evidence instead of treating every failure as storage loss.",
    href: "/library/garbage-collection-serverless",
    linkLabel: "Learn safe remote cleanup",
    icon: ShieldCheck,
    activeNodes: ["git", "crab", "bucket", "workspace"],
  },
]

const BOUNDARY_NODES = [
  { id: "git", label: "Git", icon: GitCommit, color: "#f28c52" },
  { id: "crab", label: "Crab", icon: Sparkles, color: "#4cc9d8" },
  { id: "bucket", label: "Bucket", icon: Cloud, color: "#5e87d8" },
  { id: "workspace", label: "Workspace", icon: Code2, color: "#9b87f5" },
] as const

/** Routes a repository need to the smallest Crab capability that owns it. */
export function CrabCapabilityNavigator() {
  const [capabilityId, setCapabilityId] =
    useState<Capability["id"]>("large-files")
  const panelId = useId()
  const capability =
    CAPABILITIES.find((candidate) => candidate.id === capabilityId) ??
    CAPABILITIES[0]
  const CapabilityIcon = capability.icon

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(68rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.5rem] border-2 border-[#17263b] bg-[#edf2f4] text-[#17263b] shadow-[0_28px_70px_rgba(23,38,59,0.18)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(68rem,calc(100vw-2rem))] lg:w-[min(68rem,calc(100vw-24.5rem))]">
      <header className="border-b border-[#aebcc8] px-4 py-5 sm:px-6">
        <div className="flex flex-col gap-3 lg:flex-row lg:items-end lg:justify-between">
          <div>
            <p className="m-0 font-mono text-[10px] font-black tracking-[0.18em] text-[#315e83]">
              CAPABILITY ROUTER / START WITH YOUR NEED
            </p>
            <h3 className="m-0 mt-1 text-[1.75rem] leading-tight font-black tracking-[-0.04em] sm:text-[2.15rem]">
              One problem. One first capability.
            </h3>
          </div>
          <p className="m-0 max-w-md text-sm leading-6 text-[#52657a]">
            Choose what the repository must do. The router shows the capability,
            command, ownership boundary, and next guide.
          </p>
        </div>
      </header>

      <div className="grid lg:grid-cols-[17rem_minmax(0,1fr)]">
        <div
          className="grid grid-cols-2 border-b border-[#aebcc8] bg-[#e2e9ed] sm:grid-cols-4 lg:grid-cols-1 lg:border-r lg:border-b-0"
          role="group"
          aria-label="Repository needs"
        >
          {CAPABILITIES.map((candidate) => {
            const CandidateIcon = candidate.icon
            return (
              <button
                key={candidate.id}
                type="button"
                aria-pressed={capability.id === candidate.id}
                aria-controls={panelId}
                onClick={() => setCapabilityId(candidate.id)}
                className={cn(
                  "group flex min-h-14 items-center gap-3 border-r border-b border-[#aebcc8] px-3 py-3 text-left text-xs font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#315e83] focus-visible:ring-inset motion-reduce:transition-none sm:min-h-16 lg:border-r-0",
                  capability.id === candidate.id
                    ? "bg-[#17263b] text-white"
                    : "bg-[#e2e9ed] text-[#52657a] hover:bg-white hover:text-[#17263b]"
                )}
              >
                <CandidateIcon
                  className={cn(
                    "size-4 shrink-0",
                    capability.id === candidate.id
                      ? "text-[#65cfdf]"
                      : "text-[#315e83]"
                  )}
                  aria-hidden="true"
                />
                {candidate.need}
              </button>
            )
          })}
        </div>

        <section
          id={panelId}
          className="min-w-0 bg-white p-4 sm:p-6"
          aria-live="polite"
        >
          <div className="grid gap-5 xl:grid-cols-[minmax(0,1fr)_16rem]">
            <div className="min-w-0">
              <div className="flex items-center gap-3">
                <span className="flex size-11 shrink-0 items-center justify-center rounded-xl bg-[#dff4f6] text-[#216b75]">
                  <CapabilityIcon className="size-5" aria-hidden="true" />
                </span>
                <div>
                  <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#527084]">
                    RECOMMENDATION · {capability.label.toUpperCase()}
                  </p>
                  <h4 className="m-0 mt-0.5 text-xl leading-7 font-black tracking-[-0.025em]">
                    {capability.title}
                  </h4>
                </div>
              </div>

              <p className="m-0 mt-4 text-[15px] leading-6 text-[#52657a]">
                {capability.answer}
              </p>
              <div className="mt-4 rounded-xl bg-[#17263b] p-4 text-white">
                <div className="flex items-center gap-2 font-mono text-[9px] font-black tracking-[0.14em] text-[#9eb2c9]">
                  <Terminal
                    className="size-3.5 text-[#f5a06c]"
                    aria-hidden="true"
                  />
                  FIRST COMMAND
                </div>
                <code className="mt-2 block overflow-x-auto font-mono text-[11px] leading-5 whitespace-pre-wrap text-[#b8edf2]">
                  $ {capability.command}
                </code>
              </div>

              <div className="mt-4 grid gap-3 sm:grid-cols-2">
                <div className="rounded-xl border border-[#c5d0d9] bg-[#f4f7f8] p-4">
                  <p className="m-0 font-mono text-[9px] font-black tracking-[0.14em] text-[#62778a]">
                    OWNS
                  </p>
                  <p className="m-0 mt-2 text-sm leading-5 font-black">
                    {capability.boundary}
                  </p>
                </div>
                <div className="rounded-xl border border-[#a9d8bf] bg-[#edf9f2] p-4">
                  <p className="m-0 font-mono text-[9px] font-black tracking-[0.14em] text-[#3a7557]">
                    RESULT
                  </p>
                  <p className="m-0 mt-2 text-sm leading-5 font-black text-[#28533e]">
                    {capability.result}
                  </p>
                </div>
              </div>
            </div>

            <aside className="rounded-2xl border border-[#c5d0d9] bg-[#edf2f4] p-4">
              <Network className="size-6 text-[#315e83]" aria-hidden="true" />
              <p className="m-0 mt-4 font-mono text-[9px] font-black tracking-[0.14em] text-[#62778a]">
                BOUNDARY ROUTE
              </p>
              <div className="mt-4 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1">
                {BOUNDARY_NODES.map((node, index) => {
                  const NodeIcon = node.icon
                  const active = capability.activeNodes.includes(node.id)
                  return (
                    <div key={node.id} className="contents">
                      <div className="flex flex-col items-center">
                        <span
                          className={cn(
                            "flex size-9 items-center justify-center rounded-full border-2 transition-all motion-reduce:transition-none",
                            active
                              ? "bg-white shadow-sm"
                              : "border-[#c4ced6] bg-[#e2e8ec] text-[#93a0aa]"
                          )}
                          style={
                            active
                              ? { borderColor: node.color, color: node.color }
                              : undefined
                          }
                        >
                          <NodeIcon className="size-4" aria-hidden="true" />
                        </span>
                        {index < BOUNDARY_NODES.length - 1 && (
                          <span
                            className={cn(
                              "h-4 w-0.5",
                              active ? "bg-[#7d92a5]" : "bg-[#c4ced6]"
                            )}
                            aria-hidden="true"
                          />
                        )}
                      </div>
                      <div className="pt-2 text-xs font-black">
                        {node.label}
                        <span className="ml-2 font-mono text-[9px] font-bold text-[#7a8d9d]">
                          {active ? "IN PATH" : "UNCHANGED"}
                        </span>
                      </div>
                    </div>
                  )
                })}
              </div>
              <Link
                href={capability.href}
                className="mt-5 flex min-h-11 items-center justify-between gap-3 rounded-xl bg-[#315e83] px-3 py-2 text-xs font-black text-white transition-colors hover:bg-[#234a6a] focus-visible:ring-2 focus-visible:ring-[#17263b] focus-visible:ring-offset-2 focus-visible:outline-none motion-reduce:transition-none"
              >
                {capability.linkLabel}
                <ArrowRight className="size-4 shrink-0" aria-hidden="true" />
              </Link>
            </aside>
          </div>
        </section>
      </div>
      <figcaption className="border-t border-[#aebcc8] px-4 py-3 font-mono text-[10px] text-[#62778a] sm:px-6">
        The highlighted route shows which boundaries participate. Unhighlighted
        boundaries keep their existing responsibility.
      </figcaption>
    </figure>
  )
}
