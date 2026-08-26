"use client"

import {
  ArrowRight,
  Check,
  CircleStop,
  Clock3,
  GitBranch,
  LockKeyhole,
  RadioTower,
  UnlockKeyhole,
} from "lucide-react"
import { useState } from "react"

import { cn } from "@/lib/utils"

type RaceStep = {
  label: string
  tone: "work" | "wait" | "pass" | "fail"
}

type RaceScenario = {
  id: string
  label: string
  title: string
  aliceRef: string
  bobRef: string
  alice: RaceStep[]
  bob: RaceStep[]
  result: string
  resultNote: string
}

const RACE_SCENARIOS: RaceScenario[] = [
  {
    id: "alice-wins",
    label: "Alice wins",
    title: "Both plans start from main at A",
    aliceRef: "main: A → B",
    bobRef: "main: A → C",
    alice: [
      { label: "acquire main", tone: "work" },
      { label: "upload B", tone: "work" },
      { label: "expect A = A", tone: "pass" },
      { label: "publish B", tone: "pass" },
    ],
    bob: [
      { label: "main held", tone: "wait" },
      { label: "acquire main", tone: "work" },
      { label: "expect A ≠ B", tone: "fail" },
      { label: "fetch + rebase", tone: "wait" },
    ],
    result: "main → B",
    resultNote:
      "Bob's prepared objects may remain, but his stale ref edit is rejected.",
  },
  {
    id: "lease-expires",
    label: "Alice stalls",
    title: "Alice pauses after preparing B",
    aliceRef: "main: A → B",
    bobRef: "main: A → C",
    alice: [
      { label: "acquire main", tone: "work" },
      { label: "pause past TTL", tone: "wait" },
      { label: "resume stale", tone: "work" },
      { label: "expect A ≠ C", tone: "fail" },
    ],
    bob: [
      { label: "reclaim lease", tone: "work" },
      { label: "expect A = A", tone: "pass" },
      { label: "publish C", tone: "pass" },
      { label: "release main", tone: "pass" },
    ],
    result: "main → C",
    resultNote:
      "Lease expiry restores progress; expected-old stops Alice's resumed stale plan.",
  },
  {
    id: "different-refs",
    label: "Different refs",
    title: "The writers target independent mutable keys",
    aliceRef: "main: A → B",
    bobRef: "release: R → S",
    alice: [
      { label: "acquire main", tone: "work" },
      { label: "upload B", tone: "work" },
      { label: "expect A = A", tone: "pass" },
      { label: "publish B", tone: "pass" },
    ],
    bob: [
      { label: "acquire release", tone: "work" },
      { label: "upload S", tone: "work" },
      { label: "expect R = R", tone: "pass" },
      { label: "publish S", tone: "pass" },
    ],
    result: "both publish",
    resultNote:
      "Ref-scoped leases preserve parallelism between unrelated branches.",
  },
]

const STEP_TONES = {
  work: "border-[#7190a5] bg-[#dfeaf0] text-[#17324d]",
  wait: "border-[#d59a2e] bg-[#fff0c8] text-[#6e4a06]",
  pass: "border-[#2f8f68] bg-[#dcf0e6] text-[#1f684c]",
  fail: "border-[#c34a4a] bg-[#f7dddd] text-[#913333]",
}

export function ConcurrentPushRaceBoard() {
  const [scenarioId, setScenarioId] = useState("alice-wins")
  const scenario =
    RACE_SCENARIOS.find((item) => item.id === scenarioId) ?? RACE_SCENARIOS[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-lg border-4 border-[#17324d] bg-[#d8e0e5] shadow-[0_18px_55px_rgba(23,50,77,0.2)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="grid gap-5 border-b-4 border-[#17324d] bg-[#f5f7f8] px-5 py-5 sm:px-7 lg:grid-cols-[1fr_auto] lg:items-end">
        <div>
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.2em] text-[#60798b]">
            REF SIGNAL BOX / INTERACTIVE RACE
          </p>
          <h3 className="m-0 mt-1 text-2xl font-black tracking-[-0.03em] text-[#17324d] sm:text-3xl">
            Two writers enter. One current ref edit leaves.
          </h3>
        </div>
        <div className="flex flex-wrap gap-1 rounded-md border-2 border-[#17324d] bg-white p-1">
          {RACE_SCENARIOS.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-pressed={scenario.id === item.id}
              onClick={() => setScenarioId(item.id)}
              className={cn(
                "rounded px-3 py-1.5 font-mono text-[9px] font-black outline-none focus-visible:ring-2 focus-visible:ring-[#f2b544]",
                scenario.id === item.id
                  ? "bg-[#17324d] text-white"
                  : "text-[#5f7280] hover:bg-[#eaf0f3]"
              )}
            >
              {item.label}
            </button>
          ))}
        </div>
      </header>

      <div className="p-4 sm:p-6">
        <div className="flex items-center justify-between gap-4 border-b-2 border-dashed border-[#8295a1] pb-4">
          <p className="m-0 text-lg font-black text-[#17324d]">
            {scenario.title}
          </p>
          <span className="font-mono text-[9px] font-black text-[#60798b]">
            TIME →
          </span>
        </div>

        <RaceLane
          writer="ALICE"
          refPlan={scenario.aliceRef}
          steps={scenario.alice}
          accent="bg-[#6ca8cf]"
        />
        <div className="my-3 h-1 bg-[repeating-linear-gradient(90deg,#8b9da8_0,#8b9da8_10px,transparent_10px,transparent_18px)]" />
        <RaceLane
          writer="BOB"
          refPlan={scenario.bobRef}
          steps={scenario.bob}
          accent="bg-[#d58bb6]"
        />
      </div>

      <figcaption
        className="grid gap-3 border-t-4 border-[#17324d] bg-[#17324d] px-5 py-4 text-white sm:grid-cols-[auto_1fr] sm:items-center sm:px-7"
        aria-live="polite"
      >
        <span className="inline-flex w-fit items-center gap-2 rounded-full bg-[#f2b544] px-3 py-1 font-mono text-[10px] font-black text-[#3f2c08]">
          <RadioTower className="size-4" aria-hidden="true" /> {scenario.result}
        </span>
        <span className="text-sm leading-5 text-[#c9d9e3]">
          {scenario.resultNote}
        </span>
      </figcaption>
    </figure>
  )
}

function RaceLane({
  writer,
  refPlan,
  steps,
  accent,
}: {
  writer: string
  refPlan: string
  steps: RaceStep[]
  accent: string
}) {
  return (
    <div className="grid gap-3 py-4 lg:grid-cols-[7.5rem_minmax(0,1fr)] lg:items-center">
      <div>
        <div className="flex items-center gap-2">
          <span
            className={cn(
              "size-3 rounded-full border border-[#17324d]",
              accent
            )}
          />
          <strong className="font-mono text-xs text-[#17324d]">{writer}</strong>
        </div>
        <p className="m-0 mt-1 font-mono text-[9px] font-bold text-[#617680]">
          {refPlan}
        </p>
      </div>
      <div className="grid gap-2 sm:grid-cols-4">
        {steps.map((step, index) => (
          <div
            key={`${step.label}-${index}`}
            className="relative flex items-center gap-2 sm:block"
          >
            <div
              className={cn(
                "flex min-h-16 flex-1 items-center justify-center rounded border-2 px-2 py-3 text-center font-mono text-[9px] font-black",
                STEP_TONES[step.tone]
              )}
            >
              {step.label}
            </div>
            {index < steps.length - 1 ? (
              <ArrowRight
                className="size-4 shrink-0 text-[#6b7f8a] sm:absolute sm:top-1/2 sm:-right-[13px] sm:-translate-y-1/2"
                aria-hidden="true"
              />
            ) : null}
          </div>
        ))}
      </div>
    </div>
  )
}

type LeaseMoment = {
  id: string
  label: string
  clock: string
  holder: string
  expires: number
  leaseSecs: number
  state: "live" | "expired" | "reacquired"
  action: string
  note: string
}

const LEASE_MOMENTS: LeaseMoment[] = [
  {
    id: "acquire",
    label: "Acquire",
    clock: "T+0",
    holder: "alice-7f3a",
    expires: 1700000300,
    leaseSecs: 300,
    state: "live",
    action: "Alice owns refs/heads/main",
    note: "A conditional object-store write creates the ref-scoped claim.",
  },
  {
    id: "renew",
    label: "Renew",
    clock: "T+240",
    holder: "alice-7f3a",
    expires: 1700000540,
    leaseSecs: 300,
    state: "live",
    action: "Alice extends the same claim",
    note: "Renewal checks the holder and current object version before extending the lease.",
  },
  {
    id: "expire",
    label: "Expire",
    clock: "T+541",
    holder: "alice-7f3a",
    expires: 1700000540,
    leaseSecs: 300,
    state: "expired",
    action: "The slot can be reclaimed",
    note: "Backend-authored object time determines lease age; expiry restores progress after a crash.",
  },
  {
    id: "reacquire",
    label: "Bob acquires",
    clock: "T+542",
    holder: "bob-b921",
    expires: 1700000842,
    leaseSecs: 300,
    state: "reacquired",
    action: "Bob now owns refs/heads/main",
    note: "Alice's late release cannot clear Bob's claim because release checks the holder identity.",
  },
]

export function LeaseClockInspector() {
  const [momentId, setMomentId] = useState("acquire")
  const moment =
    LEASE_MOMENTS.find((item) => item.id === momentId) ?? LEASE_MOMENTS[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(58rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden rounded-[2rem] bg-[#f2b544] p-3 shadow-[0_16px_45px_rgba(85,59,7,0.18)] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(58rem,calc(100vw-2rem))] lg:w-[min(58rem,calc(100vw-24.5rem))]">
      <div className="overflow-hidden rounded-[1.25rem] border-2 border-[#513c12] bg-[#fff9e8]">
        <header className="flex flex-wrap items-end justify-between gap-5 border-b-2 border-[#513c12] px-5 py-5 sm:px-7">
          <div>
            <p className="m-0 font-mono text-[10px] font-black tracking-[0.19em] text-[#806321]">
              FIVE-MINUTE LEASE CLOCK
            </p>
            <h3 className="m-0 mt-1 text-2xl font-black tracking-[-0.03em] text-[#3c2d0d]">
              Ownership is temporary—and holder checked.
            </h3>
          </div>
          <Clock3 className="size-9 text-[#806321]" aria-hidden="true" />
        </header>

        <div className="grid lg:grid-cols-[minmax(0,1fr)_18rem]">
          <div className="p-5 sm:p-7">
            <div className="grid grid-cols-4 gap-1 border-b-2 border-[#513c12] pb-5">
              {LEASE_MOMENTS.map((item, index) => (
                <button
                  key={item.id}
                  type="button"
                  aria-pressed={moment.id === item.id}
                  onClick={() => setMomentId(item.id)}
                  className="group relative pt-5 font-mono text-[8px] font-black text-[#705719] outline-none focus-visible:ring-2 focus-visible:ring-[#17324d]"
                >
                  <span
                    className={cn(
                      "absolute top-0 left-1/2 size-4 -translate-x-1/2 rounded-full border-2 border-[#513c12] transition-colors",
                      moment.id === item.id ? "bg-[#c34a4a]" : "bg-[#fff9e8]"
                    )}
                  />
                  {item.label}
                  {index < LEASE_MOMENTS.length - 1 ? (
                    <span className="absolute top-[7px] left-[calc(50%+8px)] h-0.5 w-[calc(100%-16px)] bg-[#8e722f]" />
                  ) : null}
                </button>
              ))}
            </div>

            <div className="mt-6 rounded-lg border-2 border-[#513c12] bg-[#17252e] p-5 text-[#dbe9ef] shadow-[inset_0_0_0_5px_#243944]">
              <div className="flex items-center justify-between gap-3">
                <span className="font-mono text-[9px] font-bold text-[#8eb1c0]">
                  locks/refs/heads/main/lock
                </span>
                <span
                  className={cn(
                    "rounded-full px-2 py-1 font-mono text-[8px] font-black",
                    moment.state === "live" && "bg-[#2f8f68] text-white",
                    moment.state === "expired" && "bg-[#c34a4a] text-white",
                    moment.state === "reacquired" &&
                      "bg-[#6ca8cf] text-[#102c3d]"
                  )}
                >
                  {moment.state.toUpperCase()}
                </span>
              </div>
              <pre className="m-0 mt-5 overflow-x-auto font-mono text-xs leading-7 text-[#eef7fa]">
                {`{
  "holder": "${moment.holder}",
  "expires_at": ${moment.expires},
  "lease_secs": ${moment.leaseSecs}
}`}
              </pre>
            </div>
          </div>

          <aside
            className="border-t-2 border-[#513c12] bg-white p-6 lg:border-t-0 lg:border-l-2"
            aria-live="polite"
          >
            <p className="m-0 font-mono text-5xl font-black tracking-[-0.08em] text-[#c34a4a]">
              {moment.clock}
            </p>
            <h4 className="m-0 mt-5 text-lg leading-6 font-black text-[#3c2d0d]">
              {moment.action}
            </h4>
            <p className="m-0 mt-2 text-sm leading-6 text-[#6d624b]">
              {moment.note}
            </p>
            <div className="mt-6 border-t border-dashed border-[#b9aa87] pt-4">
              {moment.state === "expired" ? (
                <UnlockKeyhole
                  className="size-7 text-[#c34a4a]"
                  aria-hidden="true"
                />
              ) : (
                <LockKeyhole
                  className="size-7 text-[#2f8f68]"
                  aria-hidden="true"
                />
              )}
            </div>
          </aside>
        </div>
      </div>
    </figure>
  )
}

type VisibilityStage = {
  id: string
  label: string
  writerState: string
  readerTip: "A" | "B"
  dependencies: "old" | "prepared" | "complete"
  signal: "hold" | "proceed"
  note: string
}

const VISIBILITY_STAGES: VisibilityStage[] = [
  {
    id: "upload",
    label: "Objects uploaded",
    writerState: "immutable B data exists",
    readerTip: "A",
    dependencies: "prepared",
    signal: "hold",
    note: "Uploaded packs, xorbs, and shards are not branch visibility.",
  },
  {
    id: "closure",
    label: "Closure proved",
    writerState: "every B dependency is durable",
    readerTip: "A",
    dependencies: "complete",
    signal: "hold",
    note: "B is complete, but readers still follow the committed ref state at A.",
  },
  {
    id: "prepared",
    label: "Heads prepared",
    writerState: "ref head points at invisible prepared state",
    readerTip: "A",
    dependencies: "complete",
    signal: "hold",
    note: "Prepared ref heads stay invisible until the transaction marker exists.",
  },
  {
    id: "marker",
    label: "Marker committed",
    writerState: "one immutable transaction becomes visible",
    readerTip: "B",
    dependencies: "complete",
    signal: "proceed",
    note: "The active marker is the atomic visibility boundary. Readers can now resolve B and its closure.",
  },
  {
    id: "compact",
    label: "Compacted",
    writerState: "derived manifest catches up",
    readerTip: "B",
    dependencies: "complete",
    signal: "proceed",
    note: "Compaction is cleanup after visibility; failure here does not roll B back to A.",
  },
]

export function RefVisibilitySignal() {
  const [stageId, setStageId] = useState("upload")
  const stage =
    VISIBILITY_STAGES.find((item) => item.id === stageId) ??
    VISIBILITY_STAGES[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(62rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden border-2 border-[#1d2d39] bg-[#eef2f4] shadow-[7px_7px_0_#aebac1] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(62rem,calc(100vw-2rem))] lg:w-[min(62rem,calc(100vw-24.5rem))]">
      <header className="border-b-2 border-[#1d2d39] bg-white px-5 py-5 sm:px-7">
        <p className="m-0 font-mono text-[10px] font-black tracking-[0.19em] text-[#637886]">
          READER VISIBILITY SIGNAL
        </p>
        <h3 className="m-0 mt-1 text-2xl font-black tracking-[-0.03em] text-[#17324d]">
          Data can exist before the branch points to it.
        </h3>
      </header>

      <div className="grid lg:grid-cols-[13rem_minmax(0,1fr)]">
        <div className="border-b-2 border-[#1d2d39] bg-[#17324d] p-4 lg:border-r-2 lg:border-b-0">
          <div className="grid gap-1">
            {VISIBILITY_STAGES.map((item, index) => (
              <button
                key={item.id}
                type="button"
                aria-pressed={stage.id === item.id}
                onClick={() => setStageId(item.id)}
                className={cn(
                  "grid grid-cols-[1.5rem_1fr] items-center gap-2 border px-3 py-2 text-left font-mono text-[9px] font-black outline-none focus-visible:ring-2 focus-visible:ring-[#f2b544]",
                  stage.id === item.id
                    ? "border-[#f2b544] bg-[#f2b544] text-[#3c2b08]"
                    : "border-[#4d6678] text-[#c0d1dc] hover:border-[#8ba7b8]"
                )}
              >
                <span>{String(index + 1).padStart(2, "0")}</span>
                <span>{item.label}</span>
              </button>
            ))}
          </div>
        </div>

        <div className="p-5 sm:p-7">
          <div className="grid gap-5 sm:grid-cols-[minmax(0,1fr)_10rem] sm:items-stretch">
            <div>
              <div className="grid gap-px overflow-hidden border border-[#8797a5] bg-[#8797a5] sm:grid-cols-2">
                <SignalFact label="WRITER STATE" value={stage.writerState} />
                <SignalFact
                  label="DEPENDENCIES"
                  value={
                    stage.dependencies === "complete"
                      ? "closure complete"
                      : stage.dependencies === "prepared"
                        ? "durable but unreachable"
                        : "old closure"
                  }
                />
              </div>

              <div className="mt-5 rounded-md border-2 border-[#17324d] bg-white p-5">
                <div className="flex items-center justify-between gap-3">
                  <span className="font-mono text-[9px] font-black tracking-[0.14em] text-[#667c89]">
                    READERS RESOLVE
                  </span>
                  <GitBranch
                    className="size-5 text-[#60798b]"
                    aria-hidden="true"
                  />
                </div>
                <p className="m-0 mt-3 text-4xl font-black tracking-[-0.06em] text-[#17324d]">
                  main → {stage.readerTip}
                </p>
                <p
                  className="m-0 mt-3 text-sm leading-6 text-[#5e707a]"
                  aria-live="polite"
                >
                  {stage.note}
                </p>
              </div>
            </div>

            <div
              className={cn(
                "flex min-h-56 flex-col items-center justify-center rounded-full border-8 p-4 text-center shadow-[inset_0_0_0_5px_#1d2d39]",
                stage.signal === "proceed"
                  ? "border-[#2f8f68] bg-[#d9f1e5] text-[#1f684c]"
                  : "border-[#c34a4a] bg-[#f7dddd] text-[#913333]"
              )}
            >
              {stage.signal === "proceed" ? (
                <Check className="size-12" aria-hidden="true" />
              ) : (
                <CircleStop className="size-12" aria-hidden="true" />
              )}
              <span className="mt-2 font-mono text-sm font-black">
                {stage.signal === "proceed" ? "VISIBLE" : "HOLD A"}
              </span>
            </div>
          </div>
        </div>
      </div>
    </figure>
  )
}

function SignalFact({ label, value }: { label: string; value: string }) {
  return (
    <div className="bg-white p-4">
      <p className="m-0 font-mono text-[8px] font-black tracking-[0.14em] text-[#70838e]">
        {label}
      </p>
      <p className="m-0 mt-2 text-sm leading-5 font-black text-[#263b49]">
        {value}
      </p>
    </div>
  )
}
