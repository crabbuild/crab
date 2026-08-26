"use client"

import {
  ArrowRight,
  Check,
  Database,
  Fingerprint,
  MapPin,
  Package,
  ShieldCheck,
  X,
} from "lucide-react"
import { useState } from "react"

import { cn } from "@/lib/utils"

type Chunk = {
  id: string
  size: number
  reused: boolean
}

type EditScenario = {
  id: string
  label: string
  title: string
  note: string
  reuse: string
  reusedBytes: string
  newBytes: string
  chunks: Chunk[]
}

const ORIGINAL_CHUNKS: Chunk[] = [
  { id: "A", size: 18, reused: true },
  { id: "B", size: 22, reused: true },
  { id: "C", size: 17, reused: true },
  { id: "D", size: 25, reused: true },
  { id: "E", size: 18, reused: true },
]

const EDIT_SCENARIOS: EditScenario[] = [
  {
    id: "replace",
    label: "Replace",
    title: "A fixed-size replacement disturbs one region",
    note: "The byte length stays stable, so the next content boundary can recover quickly.",
    reuse: "80%",
    reusedBytes: "8 GiB",
    newBytes: "2 GiB",
    chunks: [
      { id: "A", size: 18, reused: true },
      { id: "N", size: 22, reused: false },
      { id: "C", size: 17, reused: true },
      { id: "D", size: 25, reused: true },
      { id: "E", size: 18, reused: true },
    ],
  },
  {
    id: "insert",
    label: "Insert",
    title: "An insertion shifts boundaries, then resynchronizes",
    note: "Nearby chunks change. Stable content later in the stream finds familiar boundaries again.",
    reuse: "60%",
    reusedBytes: "6 GiB",
    newBytes: "4 GiB",
    chunks: [
      { id: "A", size: 17, reused: true },
      { id: "N", size: 13, reused: false },
      { id: "O", size: 18, reused: false },
      { id: "D", size: 28, reused: true },
      { id: "E", size: 24, reused: true },
    ],
  },
  {
    id: "append",
    label: "Append",
    title: "An append preserves every earlier boundary",
    note: "Existing chunks keep their identities. Only the new tail needs storage.",
    reuse: "83%",
    reusedBytes: "10 GiB",
    newBytes: "2 GiB",
    chunks: [...ORIGINAL_CHUNKS, { id: "N", size: 20, reused: false }],
  },
  {
    id: "recompress",
    label: "Recompress",
    title: "A new encoded stream can erase physical overlap",
    note: "The logical data may be similar, but changed compressed bytes produce new chunk identities.",
    reuse: "0%",
    reusedBytes: "0 GiB",
    newBytes: "10 GiB",
    chunks: [
      { id: "N", size: 15, reused: false },
      { id: "O", size: 21, reused: false },
      { id: "P", size: 18, reused: false },
      { id: "Q", size: 24, reused: false },
      { id: "R", size: 22, reused: false },
    ],
  },
]

export function DedupBoundaryLab() {
  const [scenarioId, setScenarioId] = useState("replace")
  const scenario =
    EDIT_SCENARIOS.find((item) => item.id === scenarioId) ?? EDIT_SCENARIOS[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(64rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden border-2 border-[#18201f] bg-[#f5f0df] shadow-[7px_7px_0_#18201f] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(64rem,calc(100vw-2rem))] lg:w-[min(64rem,calc(100vw-24.5rem))]">
      <header className="flex flex-wrap items-end justify-between gap-4 border-b-2 border-[#18201f] bg-[#fffdf6] px-4 py-4 sm:px-6">
        <div>
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.18em] text-[#5f6966]">
            CONTENT-DEFINED CUTTING MAT
          </p>
          <h3 className="m-0 mt-1 text-xl font-black tracking-tight text-[#18201f] sm:text-2xl">
            Change the edit. Watch boundaries recover.
          </h3>
        </div>
        <div className="flex flex-wrap border-2 border-[#18201f] bg-white p-1">
          {EDIT_SCENARIOS.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-pressed={scenario.id === item.id}
              onClick={() => setScenarioId(item.id)}
              className={cn(
                "px-3 py-1.5 font-mono text-[10px] font-black uppercase transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#2447a8]",
                scenario.id === item.id
                  ? "bg-[#18201f] text-white"
                  : "text-[#5f6966] hover:bg-[#ebe5d1]"
              )}
            >
              {item.label}
            </button>
          ))}
        </div>
      </header>

      <div className="grid lg:grid-cols-[minmax(0,1fr)_15rem]">
        <div className="border-b-2 border-[#18201f] p-4 sm:p-6 lg:border-r-2 lg:border-b-0">
          <ChunkRow label="VERSION 1" chunks={ORIGINAL_CHUNKS} original />
          <div className="my-4 flex items-center gap-3">
            <div className="h-px flex-1 bg-[#929b97]" />
            <span className="border border-[#18201f] bg-[#f2c14e] px-2 py-1 font-mono text-[9px] font-black uppercase">
              {scenario.label} bytes
            </span>
            <div className="h-px flex-1 bg-[#929b97]" />
          </div>
          <ChunkRow label="VERSION 2" chunks={scenario.chunks} />

          <div
            className="mt-6 border-t-2 border-[#18201f] pt-4"
            aria-live="polite"
          >
            <h4 className="m-0 text-lg font-black text-[#18201f]">
              {scenario.title}
            </h4>
            <p className="m-0 mt-1 max-w-2xl text-sm leading-6 text-[#5f6966]">
              {scenario.note}
            </p>
          </div>
        </div>

        <aside className="flex flex-col justify-between bg-[#2447a8] p-5 text-white sm:p-6">
          <div>
            <p className="m-0 font-mono text-[9px] font-black tracking-[0.16em] text-[#bdcaf1]">
              TOY REUSE MODEL
            </p>
            <p className="m-0 mt-2 font-mono text-6xl leading-none font-black tracking-[-0.08em]">
              {scenario.reuse}
            </p>
            <p className="m-0 mt-2 text-sm font-bold text-[#dce4ff]">
              source bytes reused
            </p>
          </div>
          <div className="mt-8 grid gap-2 border-t border-[#8fa3df] pt-4 font-mono text-[10px]">
            <div className="flex justify-between gap-3">
              <span className="text-[#bdcaf1]">REUSED</span>
              <strong>{scenario.reusedBytes}</strong>
            </div>
            <div className="flex justify-between gap-3">
              <span className="text-[#bdcaf1]">NEW</span>
              <strong>{scenario.newBytes}</strong>
            </div>
          </div>
        </aside>
      </div>

      <figcaption className="border-t-2 border-[#18201f] bg-[#18201f] px-4 py-3 text-xs leading-5 text-[#dce5e1] sm:px-6">
        Illustrative chunks, not measured boundaries. Crab uses a gearhash
        chunker with a 64 KiB target.
      </figcaption>
    </figure>
  )
}

function ChunkRow({
  label,
  chunks,
  original = false,
}: {
  label: string
  chunks: Chunk[]
  original?: boolean
}) {
  return (
    <div>
      <div className="mb-2 flex items-center justify-between font-mono text-[9px] font-black tracking-[0.15em] text-[#5f6966]">
        <span>{label}</span>
        <span>BYTE STREAM →</span>
      </div>
      <div className="flex min-w-0 gap-1.5">
        {chunks.map((chunk, index) => (
          <div
            key={`${chunk.id}-${index}`}
            className={cn(
              "flex h-20 min-w-10 flex-col justify-between border-2 p-2 transition-[flex-grow,background-color] duration-300 motion-reduce:transition-none",
              original || chunk.reused
                ? "border-[#16745a] bg-[#dcefe4] text-[#125c48]"
                : "border-[#e5533d] bg-[#f9ded8] text-[#9e3021]"
            )}
            style={{ flexGrow: chunk.size, flexBasis: 0 }}
          >
            <span className="font-mono text-base font-black">{chunk.id}</span>
            <span className="font-mono text-[8px] font-bold uppercase">
              {original ? "original" : chunk.reused ? "reused" : "new"}
            </span>
          </div>
        ))}
      </div>
    </div>
  )
}

type EvidenceLevel = {
  id: string
  label: string
  structure: string
  authority: string
  saves: string
  canSkipUpload: boolean
}

const EVIDENCE_LEVELS: EvidenceLevel[] = [
  {
    id: "session",
    label: "Session hit",
    structure: "in-memory chunk map",
    authority: "This process has seen the bytes.",
    saves: "Repeated hashing or local reads",
    canSkipUpload: false,
  },
  {
    id: "staging",
    label: "Staging hit",
    structure: "FilePushPlan + PlannedXorb",
    authority: "This client can reconstruct or upload the chunk.",
    saves: "Rechunking and xorb preparation",
    canSkipUpload: false,
  },
  {
    id: "canonical",
    label: "Origin proof",
    structure: "ChunkPlacement + OriginReceipt",
    authority: "A remote reader can locate durable bytes.",
    saves: "The chunk upload itself",
    canSkipUpload: true,
  },
]

export function DedupEvidenceLadder() {
  const [activeId, setActiveId] = useState("session")
  const active =
    EVIDENCE_LEVELS.find((item) => item.id === activeId) ?? EVIDENCE_LEVELS[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(62rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden border border-[#d6d9dd] bg-white min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(62rem,calc(100vw-2rem))] lg:w-[min(62rem,calc(100vw-24.5rem))]">
      <div className="grid md:grid-cols-[1fr_1.1fr]">
        <div className="border-b border-[#d6d9dd] bg-[#f3f5f7] p-4 sm:p-6 md:border-r md:border-b-0">
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.17em] text-[#687078]">
            PROOF LADDER · SELECT EVIDENCE
          </p>
          <h3 className="m-0 mt-1 text-xl font-black text-[#18201f]">
            A hash match is not yet upload permission
          </h3>

          <div className="relative mt-6 grid gap-3 before:absolute before:top-5 before:bottom-5 before:left-[1.15rem] before:w-0.5 before:bg-[#aeb6bd]">
            {EVIDENCE_LEVELS.map((item, index) => (
              <button
                key={item.id}
                type="button"
                aria-pressed={active.id === item.id}
                onClick={() => setActiveId(item.id)}
                className={cn(
                  "relative z-10 grid grid-cols-[2.25rem_1fr] items-center border p-2 text-left transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#2447a8]",
                  active.id === item.id
                    ? "border-[#2447a8] bg-white shadow-[3px_3px_0_#2447a8]"
                    : "border-[#c7cdd2] bg-[#f9fafb] hover:bg-white"
                )}
              >
                <span className="flex size-7 items-center justify-center rounded-full border-2 border-[#18201f] bg-[#f2c14e] font-mono text-[10px] font-black">
                  {index + 1}
                </span>
                <span>
                  <span className="block text-sm font-black text-[#18201f]">
                    {item.label}
                  </span>
                  <code className="mt-0.5 block text-[9px] font-bold text-[#687078]">
                    {item.structure}
                  </code>
                </span>
              </button>
            ))}
          </div>
        </div>

        <div
          className="flex flex-col justify-between p-5 sm:p-7"
          aria-live="polite"
        >
          <div>
            <div className="flex size-10 items-center justify-center border-2 border-[#18201f] bg-[#f2c14e]">
              <ShieldCheck size={19} aria-hidden="true" />
            </div>
            <p className="m-0 mt-5 font-mono text-[9px] font-black tracking-[0.16em] text-[#687078]">
              AUTHORITY AT THIS LEVEL
            </p>
            <h4 className="m-0 mt-1 text-2xl font-black tracking-tight text-[#18201f]">
              {active.authority}
            </h4>

            <div className="mt-6 grid gap-3 sm:grid-cols-2">
              <EvidenceFact
                label="WORK AVOIDED"
                value={active.saves}
                icon={Check}
              />
              <EvidenceFact
                label="SKIP REMOTE UPLOAD"
                value={
                  active.canSkipUpload
                    ? "Yes—proof is canonical"
                    : "No—durability unproven"
                }
                icon={active.canSkipUpload ? Check : X}
                positive={active.canSkipUpload}
              />
            </div>
          </div>

          <div
            className={cn(
              "mt-7 border-2 px-4 py-3 font-mono text-sm font-black tracking-[0.08em]",
              active.canSkipUpload
                ? "border-[#16745a] bg-[#dcefe4] text-[#125c48]"
                : "border-[#e5533d] bg-[#f9ded8] text-[#9e3021]"
            )}
          >
            {active.canSkipUpload ? "UPLOAD MAY BE OMITTED" : "KEEP PROVING"}
          </div>
        </div>
      </div>
      <figcaption className="border-t border-[#d6d9dd] px-4 py-3 text-xs leading-5 text-[#687078] sm:px-6">
        Local evidence can save work. Only canonical placement and origin
        evidence can prove another client can read the bytes.
      </figcaption>
    </figure>
  )
}

function EvidenceFact({
  label,
  value,
  icon: Icon,
  positive = true,
}: {
  label: string
  value: string
  icon: typeof Check
  positive?: boolean
}) {
  return (
    <div className="border border-[#c7cdd2] bg-[#f9fafb] p-3">
      <Icon
        size={16}
        className={positive ? "text-[#16745a]" : "text-[#e5533d]"}
        aria-hidden="true"
      />
      <p className="m-0 mt-3 font-mono text-[8px] font-black tracking-[0.13em] text-[#687078]">
        {label}
      </p>
      <p className="m-0 mt-1 text-xs leading-5 font-bold text-[#18201f]">
        {value}
      </p>
    </div>
  )
}

type AddressChunk = {
  id: "A" | "B" | "C"
  hash: string
  xorb: string
  index: number
  bytes: string
  positions: number[]
}

const ADDRESS_CHUNKS: AddressChunk[] = [
  {
    id: "A",
    hash: "8f21…a1",
    xorb: "xorb-31",
    index: 0,
    bytes: "64 KiB",
    positions: [1, 3],
  },
  {
    id: "B",
    hash: "d902…7c",
    xorb: "xorb-31",
    index: 1,
    bytes: "61 KiB",
    positions: [2],
  },
  {
    id: "C",
    hash: "4ab7…e9",
    xorb: "xorb-88",
    index: 4,
    bytes: "67 KiB",
    positions: [4],
  },
]

const RECIPE = ["A", "B", "A", "C"] as const

export function ChunkAddressMap() {
  const [selectedId, setSelectedId] = useState<AddressChunk["id"]>("A")
  const selected =
    ADDRESS_CHUNKS.find((chunk) => chunk.id === selectedId) ?? ADDRESS_CHUNKS[0]

  return (
    <figure className="wide-article-visual not-prose relative left-1/2 my-10 w-[min(62rem,calc(100vw-1rem))] max-w-none -translate-x-1/2 overflow-hidden border-2 border-[#18201f] bg-[#edf2ee] min-[1400px]:-translate-x-[calc(50%+1.75rem)] sm:w-[min(62rem,calc(100vw-2rem))] lg:w-[min(62rem,calc(100vw-24.5rem))]">
      <header className="border-b-2 border-[#18201f] bg-[#18201f] px-4 py-4 text-white sm:px-6">
        <p className="m-0 font-mono text-[10px] font-black tracking-[0.18em] text-[#9fb0a8]">
          CHUNK ADDRESS MAP · SELECT A RECIPE TERM
        </p>
        <h3 className="m-0 mt-1 text-xl font-black tracking-tight">
          Identity stays stable while location can move
        </h3>
      </header>

      <div className="grid gap-0 lg:grid-cols-[1fr_2rem_1fr_2rem_1fr]">
        <AddressColumn title="1 · FILE RECIPE" icon={Fingerprint}>
          <p className="m-0 text-xs leading-5 text-[#5f6966]">
            Ordered identities define the file.
          </p>
          <div className="mt-4 grid grid-cols-4 gap-1.5">
            {RECIPE.map((id, index) => (
              <button
                key={`${id}-${index}`}
                type="button"
                aria-pressed={selected.id === id}
                onClick={() => setSelectedId(id)}
                className={cn(
                  "flex h-16 flex-col justify-between border-2 p-2 text-left outline-none focus-visible:ring-2 focus-visible:ring-[#2447a8]",
                  selected.id === id
                    ? "border-[#2447a8] bg-[#dfe6fb] text-[#2447a8]"
                    : "border-[#9eaaa4] bg-white text-[#5f6966]"
                )}
              >
                <span className="font-mono text-[8px] font-bold">
                  {index + 1}
                </span>
                <strong className="font-mono text-base">{id}</strong>
              </button>
            ))}
          </div>
          <code className="mt-4 block border border-[#9eaaa4] bg-white p-2 text-[10px] font-bold text-[#18201f]">
            chunk_hash: {selected.hash}
          </code>
        </AddressColumn>

        <AddressArrow />

        <AddressColumn title="2 · CHUNK PLACEMENT" icon={MapPin}>
          <p className="m-0 text-xs leading-5 text-[#5f6966]">
            Canonical metadata maps identity to storage.
          </p>
          <div className="mt-4 border-2 border-[#16745a] bg-[#dcefe4] p-3 font-mono text-[10px] leading-6 text-[#125c48]">
            <div>chunk_hash: {selected.hash}</div>
            <div>xorb_hash: {selected.xorb}</div>
            <div>chunk_index: {selected.index}</div>
            <div>uncompressed_size: {selected.bytes}</div>
          </div>
          <p className="m-0 mt-3 text-xs font-bold text-[#18201f]">
            Recipe positions: {selected.positions.join(", ")}
          </p>
        </AddressColumn>

        <AddressArrow />

        <AddressColumn title="3 · XORB OBJECT" icon={Package}>
          <p className="m-0 text-xs leading-5 text-[#5f6966]">
            Packed bytes remain immutable and range-readable.
          </p>
          <div className="mt-5 border-2 border-[#18201f] bg-white p-2">
            <div className="flex h-20 gap-1">
              {[0, 1, 2, 3, 4].map((index) => {
                const active = selected.index === index
                return (
                  <div
                    key={index}
                    className={cn(
                      "flex flex-1 items-center justify-center border font-mono text-[10px] font-black",
                      active
                        ? "border-[#e5533d] bg-[#f9ded8] text-[#9e3021]"
                        : "border-[#c7cfc9] bg-[#edf2ee] text-[#7b8881]"
                    )}
                  >
                    {index}
                  </div>
                )
              })}
            </div>
          </div>
          <div className="mt-3 flex items-center gap-2 font-mono text-[10px] font-black text-[#18201f]">
            <Database size={14} className="text-[#e5533d]" aria-hidden="true" />
            {selected.xorb} · index {selected.index}
          </div>
        </AddressColumn>
      </div>

      <figcaption className="border-t-2 border-[#18201f] bg-[#fffdf6] px-4 py-3 text-xs leading-5 text-[#5f6966] sm:px-6">
        Chunk A appears twice in the recipe but needs one stored identity.
        Repacking may change its placement without changing the recipe hash.
      </figcaption>
    </figure>
  )
}

function AddressColumn({
  title,
  icon: Icon,
  children,
}: {
  title: string
  icon: typeof Fingerprint
  children: React.ReactNode
}) {
  return (
    <section className="min-w-0 p-4 sm:p-5">
      <div className="flex items-center gap-2 font-mono text-[10px] font-black tracking-[0.12em] text-[#18201f]">
        <Icon size={15} aria-hidden="true" />
        {title}
      </div>
      <div className="mt-4">{children}</div>
    </section>
  )
}

function AddressArrow() {
  return (
    <div className="hidden items-center justify-center border-x border-[#c7cfc9] bg-[#fffdf6] lg:flex">
      <ArrowRight size={16} className="text-[#5f6966]" aria-hidden="true" />
    </div>
  )
}
