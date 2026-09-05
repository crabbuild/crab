"use client"

import {
  ArrowDown,
  ArrowRight,
  Check,
  CircleAlert,
  Fingerprint,
  GitBranch,
  HardDrive,
  Route,
  ShieldCheck,
  X,
} from "lucide-react"
import { useState } from "react"

import { cn } from "@/lib/utils"

type MigrationPath = "existing" | "bridge" | "native"

const PATHS = {
  existing: {
    label: "Existing LFS",
    title: "Git LFS owns the transfer route",
    commit: "8f31c4d",
    commitState: "UNCHANGED",
    pointer: [
      "version https://git-lfs.github.com/spec/v1",
      "oid sha256:91ae…b72c",
      "size 8589934592",
    ],
    pointerLabel: "LFS POINTER",
    route: "Git LFS → LFS endpoint",
    storage: "one 8 GB LFS object",
    rollback: "current configuration",
    changed: ["Nothing"],
  },
  bridge: {
    label: "Crab bridge",
    title: "Crab takes the transfer call",
    commit: "8f31c4d",
    commitState: "UNCHANGED",
    pointer: [
      "version https://git-lfs.github.com/spec/v1",
      "oid sha256:91ae…b72c",
      "size 8589934592",
    ],
    pointerLabel: "SAME LFS POINTER",
    route: "Git LFS → Crab transfer agent → bucket",
    storage: "whole-file LFS semantics",
    rollback: "restore prior LFS endpoint",
    changed: ["transfer route", "Git config", "pre-push hook"],
  },
  native: {
    label: "Native Crab",
    title: "Pointer and history become Crab-native",
    commit: "c972e8a",
    commitState: "NEW ID",
    pointer: [
      "version https://crab.build/spec/v1",
      "file-hash 4cb9…731a",
      "size 8589934592",
    ],
    pointerLabel: "CRAB POINTER",
    route: "Crab filter + remote helper → bucket",
    storage: "chunks → xorbs + shards",
    rollback: "restore preserved old refs",
    changed: ["pointer", "commit IDs", "tracking rule", "storage layout"],
  },
} as const

export function LfsIdentityLab() {
  const [path, setPath] = useState<MigrationPath>("existing")
  const selected = PATHS[path]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.5rem] border border-[#7d8da4] bg-[#eef1f4] text-[#17233b] shadow-[0_24px_70px_rgba(23,35,59,0.18)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="grid gap-5 border-b border-[#aab5c3] px-5 py-5 sm:px-7 lg:grid-cols-[1fr_auto] lg:items-end">
        <div>
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-[#49617d]">
            IDENTITY LAB / 8 GB MODEL / SELECT A PATH
          </p>
          <h3 className="m-0 mt-2 text-2xl font-black tracking-[-0.04em] sm:text-3xl">
            What actually changes?
          </h3>
        </div>
        <div className="flex flex-wrap gap-2" aria-label="Migration path">
          {(Object.keys(PATHS) as MigrationPath[]).map((id) => (
            <button
              key={id}
              type="button"
              aria-pressed={path === id}
              onClick={() => setPath(id)}
              className={cn(
                "min-h-11 rounded-lg border px-3 py-2 font-mono text-[10px] font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#e56b5d] focus-visible:ring-offset-2 focus-visible:ring-offset-[#eef1f4]",
                path === id
                  ? "border-[#17233b] bg-[#17233b] text-white"
                  : "border-[#9aa8b9] bg-white text-[#49617d] hover:border-[#17233b] hover:text-[#17233b]"
              )}
            >
              {PATHS[id].label}
            </button>
          ))}
        </div>
      </header>

      <div className="grid lg:grid-cols-[1fr_18rem]" aria-live="polite">
        <section className="border-b border-[#aab5c3] p-5 sm:p-7 lg:border-r lg:border-b-0">
          <p className="m-0 text-xl font-black">{selected.title}</p>
          <div className="mt-5 grid gap-3 md:grid-cols-[1fr_auto_1fr] md:items-stretch">
            <div className="rounded-2xl bg-[#17233b] p-5 text-white">
              <div className="flex items-center justify-between gap-3">
                <span className="font-mono text-[9px] font-black tracking-[0.16em] text-[#9db1ca]">
                  GIT BLOB
                </span>
                <span className="rounded-full bg-white/10 px-2 py-1 font-mono text-[9px] font-black text-[#39a9db]">
                  {selected.pointerLabel}
                </span>
              </div>
              <pre className="m-0 mt-7 overflow-x-auto font-mono text-[10px] leading-6 whitespace-pre-wrap text-[#dce7f3]">
                {selected.pointer.join("\n")}
              </pre>
            </div>

            <ArrowRight
              className="mx-auto size-5 rotate-90 self-center text-[#7d8da4] md:rotate-0"
              aria-hidden="true"
            />

            <div className="rounded-2xl border-2 border-[#17233b] bg-white p-5">
              <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#61738a]">
                COMMIT IDENTITY
              </p>
              <div className="mt-6 flex items-center gap-3">
                <Fingerprint
                  className="size-7 text-[#e56b5d]"
                  aria-hidden="true"
                />
                <span className="font-mono text-2xl font-black">
                  {selected.commit}
                </span>
              </div>
              <span
                className={cn(
                  "mt-4 inline-flex rounded-full px-3 py-1.5 font-mono text-[9px] font-black",
                  path === "native"
                    ? "bg-[#fff0d5] text-[#76500e]"
                    : "bg-[#dcefe4] text-[#245c3a]"
                )}
              >
                {selected.commitState}
              </span>
            </div>
          </div>

          <div className="mt-3 grid gap-3 sm:grid-cols-2">
            <StateCard
              icon={Route}
              label="Transfer route"
              value={selected.route}
            />
            <StateCard
              icon={HardDrive}
              label="Storage shape"
              value={selected.storage}
            />
          </div>
        </section>

        <aside className="bg-white p-5 sm:p-7">
          <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#61738a]">
            CHANGE SEAL
          </p>
          <div className="mt-5 flex flex-wrap gap-2">
            {selected.changed.map((change) => (
              <span
                key={change}
                className={cn(
                  "rounded-md border px-2.5 py-2 font-mono text-[10px] font-black",
                  change === "Nothing"
                    ? "border-[#71b48d] bg-[#e5f4ea] text-[#245c3a]"
                    : "border-[#e8b04a] bg-[#fff4df] text-[#76500e]"
                )}
              >
                {change}
              </span>
            ))}
          </div>
          <div className="mt-7 border-t border-dashed border-[#aab5c3] pt-5">
            <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#61738a]">
              ROLLBACK BOUNDARY
            </p>
            <p className="m-0 mt-2 text-sm leading-6 font-bold">
              {selected.rollback}
            </p>
          </div>
        </aside>
      </div>
    </figure>
  )
}

function StateCard({
  icon: Icon,
  label,
  value,
}: {
  icon: typeof Route
  label: string
  value: string
}) {
  return (
    <div className="rounded-xl border border-[#aab5c3] bg-[#f8fafb] p-4">
      <Icon className="size-4 text-[#39a9db]" aria-hidden="true" />
      <p className="m-0 mt-4 font-mono text-[9px] font-black tracking-[0.14em] text-[#61738a]">
        {label.toUpperCase()}
      </p>
      <p className="m-0 mt-1 text-sm leading-6 font-bold">{value}</p>
    </div>
  )
}

type RewriteScope = "bridge" | "main" | "all"

const COMMITS = [
  { id: "A", label: "source only" },
  { id: "B", label: "model v1" },
  { id: "C", label: "model v2" },
  { id: "D", label: "release prep" },
] as const

const REWRITE_SCOPES = {
  bridge: {
    label: "Transfer bridge",
    command: "crab lfs install",
    changed: [] as string[],
    refs: "main and tags still point to the same commits",
    verdict: "No history rewrite",
  },
  main: {
    label: "Convert main",
    command: "crab lfs migrate export --include '*.safetensors' main --to-crab",
    changed: ["B", "C", "D"],
    refs: "main moves; excluded refs can still name old history",
    verdict: "B and every selected descendant get new IDs",
  },
  all: {
    label: "Convert selected history",
    command:
      "crab lfs migrate export --include '*.safetensors' --to-crab --everything --object-map ../lfs-to-crab.csv",
    changed: ["B", "C", "D"],
    refs: "all selected branches and tags need mapped destinations",
    verdict: "The object map becomes cutover evidence",
  },
} as const

export function LfsRewriteMap() {
  const [scope, setScope] = useState<RewriteScope>("bridge")
  const selected = REWRITE_SCOPES[scope]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.5rem] bg-[#17233b] text-white shadow-[0_24px_70px_rgba(23,35,59,0.2)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="border-b border-[#42536d] px-5 py-5 sm:px-7">
        <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-[#39a9db]">
          DESCENDANT MAP / MODEL FIRST APPEARS AT B
        </p>
        <div className="mt-3 flex flex-wrap gap-2" aria-label="Rewrite scope">
          {(Object.keys(REWRITE_SCOPES) as RewriteScope[]).map((id) => (
            <button
              key={id}
              type="button"
              aria-pressed={scope === id}
              onClick={() => setScope(id)}
              className={cn(
                "min-h-11 rounded-full border px-3 py-2 font-mono text-[10px] font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#e8b04a] focus-visible:ring-offset-2 focus-visible:ring-offset-[#17233b]",
                scope === id
                  ? "border-[#e8b04a] bg-[#e8b04a] text-[#17233b]"
                  : "border-[#526580] text-[#c8d3e2] hover:border-[#e8b04a] hover:text-white"
              )}
            >
              {REWRITE_SCOPES[id].label}
            </button>
          ))}
        </div>
      </header>

      <div
        className="grid gap-6 p-5 sm:p-7 lg:grid-cols-[1fr_20rem]"
        aria-live="polite"
      >
        <section>
          <div className="overflow-x-auto pb-2">
            <div className="flex min-w-[34rem] items-center">
              {COMMITS.map((commit, index) => {
                const changed = (
                  selected.changed as readonly string[]
                ).includes(commit.id)
                return (
                  <div key={commit.id} className="contents">
                    <div className="w-28 shrink-0 text-center">
                      <div
                        className={cn(
                          "mx-auto flex size-16 items-center justify-center rounded-full border-2 font-mono text-xl font-black",
                          changed
                            ? "border-[#e8b04a] bg-[#fff0d5] text-[#76500e]"
                            : "border-[#71b48d] bg-[#24483a] text-[#a8dfba]"
                        )}
                      >
                        {changed ? `${commit.id}′` : commit.id}
                      </div>
                      <p className="m-0 mt-3 font-mono text-[9px] text-[#aab9cb]">
                        {commit.label}
                      </p>
                    </div>
                    {index < COMMITS.length - 1 ? (
                      <div className="h-0.5 flex-1 bg-[#526580]" />
                    ) : null}
                  </div>
                )
              })}
            </div>
          </div>

          <div className="mt-6 rounded-xl border border-[#526580] bg-[#202e47] p-4">
            <p className="m-0 font-mono text-[9px] font-black tracking-[0.14em] text-[#8da2bc]">
              COMMAND
            </p>
            <code className="mt-2 block overflow-x-auto font-mono text-[11px] leading-6 whitespace-pre-wrap text-white">
              {selected.command}
            </code>
          </div>
        </section>

        <aside className="rounded-2xl bg-[#eef1f4] p-5 text-[#17233b]">
          <GitBranch className="size-6 text-[#e56b5d]" aria-hidden="true" />
          <p className="m-0 mt-5 text-lg leading-6 font-black">
            {selected.verdict}
          </p>
          <p className="m-0 mt-3 text-sm leading-6 text-[#52637a]">
            {selected.refs}
          </p>
          <div className="mt-5 border-t border-dashed border-[#aab5c3] pt-4">
            <p className="m-0 font-mono text-[9px] font-black tracking-[0.14em] text-[#61738a]">
              KEY STRUCTURES
            </p>
            <div className="mt-3 flex flex-wrap gap-2">
              {["Git blob", "commit graph", "refs", "object-map.csv"].map(
                (item) => (
                  <span
                    key={item}
                    className="rounded-md border border-[#aab5c3] bg-white px-2 py-1.5 font-mono text-[9px] font-bold"
                  >
                    {item}
                  </span>
                )
              )}
            </div>
          </div>
        </aside>
      </div>
    </figure>
  )
}

type RehearsalCase = "healthy" | "missing" | "mixed" | "rewrite"

const REHEARSAL_CASES = {
  healthy: {
    label: "Bridge passes",
    title: "New writes and cold reads both work",
    result: "READY FOR A LIMITED BRIDGE",
    pass: true,
    steps: [
      ["Inventory", "LFS objects available", true],
      ["Upload", "new object reaches Crab storage", true],
      ["Cold clone", "empty cache fetches the object", true],
      ["Integrity", "crab lfs fsck passes", true],
    ],
    action: "Keep the old endpoint during the evidence window.",
  },
  missing: {
    label: "Old tag fails",
    title: "A historical pointer has no source bytes",
    result: "STOP: REPAIR SOURCE CLOSURE",
    pass: false,
    steps: [
      ["Inventory", "release/v1 pointer found", true],
      ["Resolve", "OID absent locally and remotely", false],
      ["Convert", "cannot manufacture missing bytes", false],
      ["Publish", "blocked before ref changes", false],
    ],
    action: "Recover that OID or remove the ref from the supported scope.",
  },
  mixed: {
    label: "Plain client",
    title: "A client lacks Crab transfer configuration",
    result: "DEFINE THE MIXED-CLIENT POLICY",
    pass: false,
    steps: [
      ["Clone", "Git history arrives", true],
      ["Smudge", "LFS pointer is recognized", true],
      ["Transfer", "Crab agent is unavailable", false],
      ["Checkout", "large file stays unresolved", false],
    ],
    action:
      "Install Crab in bootstrap and CI, or retain a supported LFS route.",
  },
  rewrite: {
    label: "Native passes",
    title: "A fresh clone proves the rewritten repository",
    result: "READY FOR COORDINATED CUTOVER",
    pass: true,
    steps: [
      ["Map", "old and new commit IDs recorded", true],
      ["Hydrate", "model bytes match before hash", true],
      ["Topology", "selected refs close correctly", true],
      ["Workload", "build passes from empty cache", true],
    ],
    action:
      "Freeze writes, publish mapped refs, then test one independent client.",
  },
} as const

export function LfsRehearsalConsole() {
  const [caseId, setCaseId] = useState<RehearsalCase>("healthy")
  const selected = REHEARSAL_CASES[caseId]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[1.5rem] border-2 border-[#17233b] bg-white text-[#17233b] shadow-[0_24px_70px_rgba(23,35,59,0.15)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="bg-[#eef1f4] px-5 py-5 sm:px-7">
        <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-[#49617d]">
          CUTOVER REHEARSAL / CHOOSE A FAILURE OR SUCCESS
        </p>
        <div className="mt-3 flex flex-wrap gap-2" aria-label="Rehearsal case">
          {(Object.keys(REHEARSAL_CASES) as RehearsalCase[]).map((id) => (
            <button
              key={id}
              type="button"
              aria-pressed={caseId === id}
              onClick={() => setCaseId(id)}
              className={cn(
                "min-h-11 rounded-lg border px-3 py-2 font-mono text-[10px] font-black transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#39a9db] focus-visible:ring-offset-2 focus-visible:ring-offset-[#eef1f4]",
                caseId === id
                  ? "border-[#17233b] bg-[#17233b] text-white"
                  : "border-[#9aa8b9] bg-white text-[#49617d] hover:border-[#17233b]"
              )}
            >
              {REHEARSAL_CASES[id].label}
            </button>
          ))}
        </div>
      </header>

      <div className="grid lg:grid-cols-[1fr_19rem]" aria-live="polite">
        <section className="border-b border-[#aab5c3] p-5 sm:p-7 lg:border-r lg:border-b-0">
          <h3 className="m-0 text-xl font-black tracking-[-0.03em] sm:text-2xl">
            {selected.title}
          </h3>
          <div className="mt-6 grid gap-2 sm:grid-cols-2 xl:grid-cols-4">
            {selected.steps.map(([label, detail, passed], index) => (
              <div key={label} className="contents">
                <div
                  className={cn(
                    "relative rounded-xl border p-4",
                    passed
                      ? "border-[#71b48d] bg-[#edf8f1]"
                      : "border-[#e56b5d] bg-[#fff0ed]"
                  )}
                >
                  {passed ? (
                    <Check
                      className="size-5 text-[#2f7449]"
                      aria-hidden="true"
                    />
                  ) : (
                    <X className="size-5 text-[#a23d33]" aria-hidden="true" />
                  )}
                  <p className="m-0 mt-5 font-mono text-[10px] font-black">
                    {label}
                  </p>
                  <p className="m-0 mt-2 text-xs leading-5 text-[#52637a]">
                    {detail}
                  </p>
                  {index < selected.steps.length - 1 ? (
                    <ArrowDown
                      className="absolute -bottom-5 left-1/2 z-10 size-4 -translate-x-1/2 text-[#7d8da4] sm:hidden"
                      aria-hidden="true"
                    />
                  ) : null}
                </div>
              </div>
            ))}
          </div>
        </section>

        <aside
          className={cn(
            "p-5 sm:p-7",
            selected.pass ? "bg-[#dcefe4]" : "bg-[#fff0d5]"
          )}
        >
          {selected.pass ? (
            <ShieldCheck className="size-7 text-[#2f7449]" aria-hidden="true" />
          ) : (
            <CircleAlert className="size-7 text-[#a86913]" aria-hidden="true" />
          )}
          <p className="m-0 mt-5 font-mono text-[10px] leading-5 font-black tracking-[0.12em]">
            {selected.result}
          </p>
          <p className="m-0 mt-3 text-sm leading-6 font-bold">
            {selected.action}
          </p>
        </aside>
      </div>
    </figure>
  )
}
