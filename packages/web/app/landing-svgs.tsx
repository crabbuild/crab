"use client"

/* SVGs use unified design tokens from globals.css for automatic
   dark/light mode adaptation. Tokens consumed:
   --primary, --primary-muted, --foreground, --muted-foreground,
   --border, --muted, --card */

const P  = "var(--primary)"
const PL = "var(--primary-muted)"
const N1 = "var(--foreground)"
const N3 = "var(--muted-foreground)"
const NB = "var(--border)"
const NS = "var(--muted)"

const ArrMarker = ({ id }: { id: string }) => (
  <defs>
    <marker id={id} markerWidth="7" markerHeight="5" refX="7" refY="2.5" orient="auto">
      <path d="M0 0L7 2.5L0 5" fill="none" stroke={NB} strokeWidth="1"/>
    </marker>
  </defs>
)

export function HeroDiagramSvg() {
  return (
    <div className="mx-auto w-full max-w-[900px]" role="img" aria-label="Crab architecture: Git Repo to Crab engine to Cloud Storage">
      {/* Title */}
      <p className="mb-8 text-center text-base font-semibold text-foreground md:text-lg">
        Serverless Git for Large Files
      </p>

      {/* 3-column layout */}
      <div className="grid grid-cols-1 items-center gap-4 md:grid-cols-[1fr_auto_1.2fr_auto_1fr]">

        {/* Git Repo */}
        <div className="rounded-xl border border-border bg-muted/50 px-6 py-6">
          <h4 className="mb-4 text-center text-sm font-semibold text-foreground">Git Repo</h4>
          <div className="flex items-start gap-4">
            {/* Branch viz */}
            <div className="flex flex-col items-center gap-0">
              <div className="h-3 w-3 rounded-full border-2 border-primary bg-card" />
              <div className="h-5 w-0.5 bg-primary" />
              <div className="relative">
                <div className="h-3 w-3 rounded-full border-2 border-primary bg-card" />
                <div className="absolute -right-3 -top-2 h-2 w-2 rounded-full bg-primary" />
              </div>
              <div className="h-5 w-0.5 bg-primary" />
              <div className="h-3 w-3 rounded-full border-2 border-primary bg-card" />
            </div>
            {/* Labels */}
            <div className="flex flex-col gap-2 pt-0.5">
              <span className="text-xs text-muted-foreground">commits</span>
              <span className="text-xs text-muted-foreground">branches</span>
              <span className="text-xs text-muted-foreground">pointer blobs</span>
            </div>
          </div>
        </div>

        {/* Arrow 1 */}
        <div className="flex flex-col items-center gap-1 px-2">
          <span className="text-[10px] text-muted-foreground">push / pull</span>
          <svg width="48" height="12" viewBox="0 0 48 12" className="text-border">
            <line x1="0" y1="6" x2="42" y2="6" stroke="currentColor" strokeWidth="1.5"/>
            <path d="M38 2L46 6L38 10" fill="none" stroke="currentColor" strokeWidth="1.5"/>
          </svg>
        </div>

        {/* Crab Engine */}
        <div className="rounded-xl border-2 border-primary bg-primary-muted px-6 py-5">
          <h4 className="mb-1 text-center text-base font-bold text-primary">Crab</h4>
          <p className="mb-4 text-center text-[11px] text-muted-foreground">git remote helper</p>
          <div className="flex flex-col gap-2">
            <div className="rounded-lg border border-border bg-card px-4 py-2 text-center text-xs text-muted-foreground">
              CDC chunking
            </div>
            <div className="rounded-lg border border-border bg-card px-4 py-2 text-center text-xs text-muted-foreground">
              Dedup &amp; pack
            </div>
            <div className="rounded-lg border border-border bg-card px-4 py-2 text-center text-xs text-muted-foreground">
              Filter &amp; VFS
            </div>
          </div>
        </div>

        {/* Arrow 2 */}
        <div className="flex flex-col items-center gap-1 px-2">
          <span className="text-[10px] text-muted-foreground">xorbs / shards</span>
          <svg width="48" height="12" viewBox="0 0 48 12" className="text-border">
            <line x1="0" y1="6" x2="42" y2="6" stroke="currentColor" strokeWidth="1.5"/>
            <path d="M38 2L46 6L38 10" fill="none" stroke="currentColor" strokeWidth="1.5"/>
          </svg>
        </div>

        {/* Cloud Storage */}
        <div className="rounded-xl border border-border bg-muted/50 px-6 py-6">
          <h4 className="mb-4 text-center text-sm font-semibold text-foreground">Cloud Storage</h4>
          {/* Cloud icon */}
          <div className="mb-3 flex justify-center">
            <svg width="40" height="28" viewBox="0 0 40 28" fill="none" className="text-border">
              <path d="M10 20C6 20 4 17 5 14C6 11 9 10 12 10.5C13 7 16 5 20 5C24 5 27 7 28 10.5C31 10 34 11 35 14C36 17 34 20 30 20Z"
                stroke="currentColor" strokeWidth="1.5" strokeLinejoin="round" fill="none"/>
            </svg>
          </div>
          <div className="flex flex-col items-center gap-1">
            <span className="text-xs text-muted-foreground">AWS S3</span>
            <span className="text-xs text-muted-foreground">Google Cloud Storage</span>
            <span className="text-xs text-muted-foreground">Azure Blob</span>
          </div>
        </div>
      </div>

      {/* Bottom labels */}
      <div className="mt-5 grid grid-cols-1 gap-4 md:grid-cols-[1fr_auto_1.2fr_auto_1fr]">
        <p className="text-center text-[11px] text-muted-foreground">Unmodified Git workflow</p>
        <div />
        <p className="text-center text-[11px] text-muted-foreground">Single binary, no servers</p>
        <div />
        <p className="text-center text-[11px] text-muted-foreground">Your bucket, your data</p>
      </div>
    </div>
  )
}

export function ChunkingDiagramSvg() {
  return (
    <svg viewBox="0 0 440 340" fill="none" xmlns="http://www.w3.org/2000/svg" role="img" aria-label="Content-defined chunking: original file split into chunks, deduplicated, then packed into xorbs">
      {/* File */}
      <text x="20" y="24" fill={N1} fontSize="12" fontWeight="600">Original File</text>
      <rect x="20" y="34" width="400" height="24" rx="5" fill={NS} stroke={NB} strokeWidth="1"/>
      <text x="220" y="50" textAnchor="middle" fill={N1} fontSize="10">continuous byte stream</text>

      {/* arrow */}
      <line x1="220" y1="64" x2="220" y2="88" stroke={NB} strokeWidth="1.5" markerEnd="url(#ca)"/>
      <text x="260" y="80" fill={P} fontSize="10" fontWeight="600">Gearhash CDC</text>

      {/* Chunks */}
      <text x="20" y="106" fill={N1} fontSize="12" fontWeight="600">Content-Defined Chunks</text>
      {[0,1,2,3,4,5,6,7].map(i => {
        const dup = [0,3,5,7].includes(i)
        return (
          <g key={i}>
            <rect x={20 + i * 50} y={116} width={44} height={24} rx="4"
              fill={dup ? PL : NS}
              stroke={dup ? P : NB}
              strokeWidth="1.2" />
            <text x={20 + i * 50 + 22} y={132} textAnchor="middle"
              fill={dup ? P : N3} fontSize="9" fontWeight="600">
              {`C${i + 1}`}
            </text>
          </g>
        )
      })}

      {/* arrow */}
      <line x1="220" y1="150" x2="220" y2="174" stroke={NB} strokeWidth="1.5" markerEnd="url(#ca)"/>
      <text x="260" y="166" fill={P} fontSize="10" fontWeight="600">3-tier dedup</text>

      {/* After dedup */}
      <text x="20" y="192" fill={N1} fontSize="12" fontWeight="600">After Deduplication</text>
      {[0,1,2,3].map(i => (
        <g key={i}>
          <rect x={20 + i * 50} y={202} width={44} height={24} rx="4"
            fill={PL} stroke={P} strokeWidth="1.2" />
          <text x={20 + i * 50 + 22} y={218} textAnchor="middle"
            fill={P} fontSize="9" fontWeight="600">
            {["C2","C3","C4","C6"][i]}
          </text>
        </g>
      ))}
      <text x="230" y={218} fill={N3} fontSize="10">← 4 unique chunks uploaded</text>
      <text x="230" y={232} fill={N3} fontSize="10">   4 duplicates skipped (50% saved)</text>

      {/* arrow */}
      <line x1="220" y1="248" x2="220" y2="272" stroke={NB} strokeWidth="1.5" markerEnd="url(#ca)"/>

      {/* Xorb */}
      <rect x="100" y="280" width="240" height="32" rx="7" fill={PL} stroke={P} strokeWidth="1.2"/>
      <text x="220" y="300" textAnchor="middle" fill={P} fontSize="11" fontWeight="600">
        Packed into ~64 MiB xorbs → S3
      </text>

      <ArrMarker id="ca" />
    </svg>
  )
}

export function PipelineDiagramSvg() {
  const stages = [
    { x: 20, y: 20, w: 100, label: "download" },
    { x: 170, y: 20, w: 100, label: "clean" },
    { x: 320, y: 20, w: 100, label: "featurize" },
    { x: 170, y: 110, w: 100, label: "train-a" },
    { x: 320, y: 110, w: 100, label: "train-b" },
    { x: 245, y: 200, w: 100, label: "evaluate" },
  ]
  const edges: [number,number][] = [[0,1],[1,2],[2,3],[2,4],[3,5],[4,5]]
  return (
    <svg viewBox="0 0 440 270" fill="none" xmlns="http://www.w3.org/2000/svg" role="img" aria-label="DAG pipeline: download, clean, featurize, parallel train-a and train-b, evaluate">
      {edges.map(([f,t], i) => (
        <line key={i}
          x1={stages[f].x + stages[f].w / 2} y1={stages[f].y + 18}
          x2={stages[t].x + stages[t].w / 2} y2={stages[t].y + 18}
          stroke={NB} strokeWidth="1.5" markerEnd="url(#pa)" />
      ))}
      {stages.map((s, i) => (
        <g key={i}>
          <rect x={s.x} y={s.y} width={s.w} height="36" rx="7"
            fill={PL} stroke={P} strokeWidth="1.2" />
          <text x={s.x + s.w / 2} y={s.y + 22} textAnchor="middle"
            fill={P} fontSize="11" fontWeight="600">{s.label}</text>
        </g>
      ))}
      {/* parallel bracket */}
      <rect x="156" y="96" width="278" height="60" rx="10" fill="none"
        stroke={P} strokeWidth="1" strokeDasharray="4 3" opacity="0.3" />
      <text x="295" y="92" textAnchor="middle" fill={P} fontSize="9" fontWeight="500" opacity="0.6">
        parallel execution
      </text>

      <ArrMarker id="pa" />
    </svg>
  )
}
