import {
  BrainCircuit,
  Check,
  Cloud,
  Code2,
  Database,
  File,
  FileBox,
  FileCode2,
  FileSpreadsheet,
  Film,
  FolderOpen,
  GitBranch,
  HardDrive,
  Image,
  Layers3,
  LockKeyhole,
  PackageCheck,
  Play,
  ServerCog,
  ShieldCheck,
  Terminal,
  Workflow,
} from "lucide-react"

function Window({
  label,
  children,
}: {
  label: string
  children: React.ReactNode
}) {
  return (
    <div className="overflow-hidden rounded-3xl border border-border bg-card shadow-2xl shadow-primary/10">
      <div className="flex items-center justify-between border-b border-border bg-muted/50 px-5 py-3">
        <div className="flex items-center gap-1.5" aria-hidden="true">
          <span className="size-2 rounded-full bg-foreground/15" />
          <span className="size-2 rounded-full bg-foreground/15" />
          <span className="size-2 rounded-full bg-primary/70" />
        </div>
        <span className="font-mono text-[10px] font-medium tracking-[0.14em] text-muted-foreground uppercase">
          {label}
        </span>
      </div>
      {children}
    </div>
  )
}

function FlowArrow({ label }: { label?: string }) {
  return (
    <div className="flex min-w-8 flex-1 items-center gap-2" aria-hidden="true">
      <span className="h-px flex-1 bg-linear-to-r from-border to-primary/70" />
      {label ? (
        <span className="hidden text-[9px] font-semibold tracking-wider whitespace-nowrap text-primary uppercase sm:inline">
          {label}
        </span>
      ) : null}
      <span className="size-1.5 rotate-45 border-t border-r border-primary" />
    </div>
  )
}

function Node({
  icon: Icon,
  title,
  caption,
  active = false,
}: {
  icon: typeof File
  title: string
  caption: string
  active?: boolean
}) {
  return (
    <div
      className={
        active
          ? "min-w-0 rounded-xl border border-primary/40 bg-primary/10 p-3 shadow-sm shadow-primary/10"
          : "min-w-0 rounded-xl border border-border bg-background/70 p-3"
      }
    >
      <Icon
        aria-hidden="true"
        size={16}
        className={active ? "text-primary" : "text-muted-foreground"}
      />
      <p className="mt-2 truncate text-xs font-semibold text-foreground">
        {title}
      </p>
      <p className="mt-0.5 truncate text-[10px] text-muted-foreground">
        {caption}
      </p>
    </div>
  )
}

export function UseCasesOverviewVisual() {
  return (
    <Window label="one commit · complete project">
      <div
        className="relative p-5 sm:p-7"
        role="img"
        aria-label="A Git commit connects code and large-file pointers to Crab, which stores content-addressed chunks in the team's cloud bucket."
      >
        <div className="absolute inset-0 [background-image:radial-gradient(var(--border)_1px,transparent_1px)] [background-size:18px_18px] opacity-50" />
        <div className="relative rounded-2xl border border-border bg-background/85 p-4 shadow-sm backdrop-blur">
          <div className="flex items-center justify-between gap-3">
            <div className="flex items-center gap-3">
              <span className="flex size-10 items-center justify-center rounded-xl bg-foreground text-background">
                <GitBranch aria-hidden="true" size={18} />
              </span>
              <div>
                <p className="text-sm font-semibold text-foreground">
                  commit a18f7e2
                </p>
                <p className="text-[11px] text-muted-foreground">
                  experiment/vision-v4
                </p>
              </div>
            </div>
            <span className="rounded-full border border-primary/20 bg-primary/10 px-2.5 py-1 text-[10px] font-semibold text-primary">
              complete state
            </span>
          </div>
          <div className="mt-4 grid grid-cols-2 gap-3">
            <Node icon={Code2} title="train.py" caption="ordinary Git blob" />
            <Node
              icon={FileBox}
              title="model.safetensors"
              caption="Crab pointer blob"
              active
            />
          </div>
        </div>

        <div className="relative my-3 flex items-center px-8">
          <FlowArrow label="push" />
        </div>

        <div className="relative grid grid-cols-[1fr_auto_1fr] items-center gap-3">
          <div className="rounded-2xl border-2 border-primary/40 bg-primary/10 p-4 shadow-lg shadow-primary/10">
            <div className="flex items-center gap-2 text-primary">
              <Layers3 aria-hidden="true" size={17} />
              <span className="text-xs font-bold">Crab</span>
            </div>
            <div className="mt-3 space-y-2 text-[10px] text-foreground/80">
              <p className="rounded-md border border-primary/20 bg-card px-2 py-1.5">
                chunk · deduplicate
              </p>
              <p className="rounded-md border border-primary/20 bg-card px-2 py-1.5">
                verify · reconstruct
              </p>
            </div>
          </div>
          <FlowArrow />
          <div className="rounded-2xl border border-border bg-background/80 p-4">
            <div className="flex items-center gap-2">
              <Cloud aria-hidden="true" size={17} className="text-primary" />
              <span className="text-xs font-bold text-foreground">
                Your bucket
              </span>
            </div>
            <div className="mt-3 flex flex-wrap gap-1.5">
              {["S3", "GCS", "Azure", "S3-compatible"].map((provider) => (
                <span
                  key={provider}
                  className="rounded bg-muted px-2 py-1 text-[9px] text-muted-foreground"
                >
                  {provider}
                </span>
              ))}
            </div>
          </div>
        </div>

        <div className="relative mt-5 flex items-center justify-center gap-2 text-[10px] text-muted-foreground">
          <LockKeyhole aria-hidden="true" size={12} className="text-primary" />
          Git keeps the timeline · object storage keeps the payloads
        </div>
      </div>
    </Window>
  )
}

const modelRows = [
  {
    branch: "main",
    label: "base checkpoint",
    chunks: [1, 1, 1, 1, 1, 1, 1, 1],
  },
  {
    branch: "exp/vision",
    label: "fine-tune",
    chunks: [0, 1, 1, 1, 1, 0, 1, 1],
  },
  {
    branch: "release/v4",
    label: "promoted model",
    chunks: [0, 1, 1, 1, 1, 0, 1, 1],
  },
]

export function ModelLineageVisual() {
  return (
    <Window label="model lineage">
      <div
        className="p-5 sm:p-7"
        role="img"
        aria-label="Three Git refs share most model chunks. Only chunks not already stored are uploaded, and the release ref resolves to a complete checkpoint."
      >
        <div className="flex items-center justify-between gap-4">
          <div>
            <p className="text-sm font-semibold text-foreground">
              One model, three refs
            </p>
            <p className="mt-1 text-xs text-muted-foreground">
              Shared chunks stay content-addressed
            </p>
          </div>
          <span className="flex items-center gap-1.5 rounded-full bg-primary/10 px-2.5 py-1 text-[10px] font-semibold text-primary">
            <BrainCircuit aria-hidden="true" size={12} /> lineage intact
          </span>
        </div>

        <div className="relative mt-7 space-y-5">
          <span
            aria-hidden="true"
            className="absolute top-3 bottom-3 left-[6px] w-px bg-border"
          />
          {modelRows.map((row, rowIndex) => (
            <div
              key={row.branch}
              className="relative grid grid-cols-[14px_1fr] gap-3"
            >
              <span
                className={
                  "z-10 mt-2 size-3 rounded-full border-2 border-card " +
                  (rowIndex === 2 ? "bg-primary" : "bg-muted-foreground")
                }
              />
              <div className="rounded-xl border border-border bg-muted/30 p-3.5">
                <div className="flex items-center justify-between gap-3">
                  <div>
                    <p className="font-mono text-[10px] font-semibold text-primary">
                      {row.branch}
                    </p>
                    <p className="mt-0.5 text-xs font-medium text-foreground">
                      {row.label}
                    </p>
                  </div>
                  <span className="text-[9px] text-muted-foreground">
                    {rowIndex === 0
                      ? "initial chunks"
                      : rowIndex === 1
                        ? "reuse + new"
                        : "same snapshot"}
                  </span>
                </div>
                <div className="mt-3 grid grid-cols-8 gap-1.5">
                  {row.chunks.map((reused, index) => (
                    <span
                      key={row.branch + "-" + index}
                      className={
                        reused
                          ? "h-5 rounded-sm border border-border bg-muted"
                          : "h-5 rounded-sm border border-primary/50 bg-primary shadow-sm shadow-primary/20"
                      }
                    />
                  ))}
                </div>
              </div>
            </div>
          ))}
        </div>

        <div className="mt-5 flex flex-wrap gap-4 border-t border-border pt-4 text-[10px] text-muted-foreground">
          <span className="flex items-center gap-1.5">
            <span className="size-2 rounded-sm bg-muted" /> already stored
          </span>
          <span className="flex items-center gap-1.5">
            <span className="size-2 rounded-sm bg-primary" /> new content
          </span>
          <span className="ml-auto flex items-center gap-1.5 text-primary">
            <Check size={11} /> release reconstructs exactly
          </span>
        </div>
      </div>
    </Window>
  )
}

export function DataSnapshotVisual() {
  return (
    <Window label="research snapshot">
      <div
        className="p-5 sm:p-7"
        role="img"
        aria-label="A Git ref identifies a notebook, parameters, schema, and Crab-managed dataset snapshot. A selected validation partition is hydrated into the workspace."
      >
        <div className="rounded-2xl border border-primary/30 bg-primary/8 p-4">
          <div className="flex items-center gap-3">
            <span className="flex size-10 items-center justify-center rounded-xl bg-primary text-primary-foreground">
              <GitBranch aria-hidden="true" size={18} />
            </span>
            <div>
              <p className="text-sm font-semibold text-foreground">
                release/q3-analysis
              </p>
              <p className="text-[11px] text-muted-foreground">
                one ref · complete analytical state
              </p>
            </div>
          </div>
        </div>

        <div className="mx-auto h-5 w-px bg-primary/40" aria-hidden="true" />
        <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
          <Node icon={FileCode2} title="analysis.ipynb" caption="Git blob" />
          <Node icon={FileSpreadsheet} title="params.yaml" caption="Git blob" />
          <Node
            icon={Database}
            title="train.parquet"
            caption="Crab pointer"
            active
          />
          <Node
            icon={FileBox}
            title="imagery/"
            caption="Crab pointers"
            active
          />
        </div>

        <div className="mt-5 rounded-2xl border border-border bg-muted/30 p-4">
          <div className="flex items-center justify-between gap-3">
            <div className="flex items-center gap-2">
              <FolderOpen
                aria-hidden="true"
                size={16}
                className="text-primary"
              />
              <span className="text-xs font-semibold text-foreground">
                Local workspace
              </span>
            </div>
            <span className="rounded-full bg-primary/10 px-2 py-1 text-[9px] font-semibold text-primary">
              selective
            </span>
          </div>
          <div className="mt-3 space-y-2 font-mono text-[10px]">
            <p className="flex items-center justify-between rounded-md bg-card px-3 py-2 text-foreground">
              <span>datasets/validation/</span>
              <span className="text-primary">hydrated</span>
            </p>
            <p className="flex items-center justify-between rounded-md border border-dashed border-border px-3 py-2 text-muted-foreground">
              <span>datasets/training/</span>
              <span>pointer</span>
            </p>
          </div>
        </div>

        <p className="mt-4 text-center text-[10px] text-muted-foreground">
          Switching refs changes both the analysis and the exact data snapshot
          it names.
        </p>
      </div>
    </Window>
  )
}

const creativeAssets = [
  { icon: Image, name: "textures/", state: "local", active: true },
  { icon: Film, name: "cinematics/", state: "pointer", active: false },
  { icon: FileBox, name: "meshes/", state: "local", active: true },
  { icon: File, name: "audio/", state: "pointer", active: false },
  { icon: Image, name: "references/", state: "pointer", active: false },
  { icon: FileBox, name: "lighting/", state: "local", active: true },
]

export function CreativeWorkspaceVisual() {
  return (
    <Window label="working set">
      <div
        className="p-5 sm:p-7"
        role="img"
        aria-label="A creative workspace has selected asset folders hydrated locally while other folders remain lightweight pointers. Clean hydrated assets can be dehydrated to reclaim disk."
      >
        <div className="flex items-center justify-between gap-3">
          <div>
            <p className="text-sm font-semibold text-foreground">
              world-building / desert-city
            </p>
            <p className="mt-1 text-xs text-muted-foreground">
              Only the active scene is materialized
            </p>
          </div>
          <HardDrive aria-hidden="true" size={19} className="text-primary" />
        </div>

        <div className="mt-6 grid grid-cols-2 gap-3 sm:grid-cols-3">
          {creativeAssets.map((asset) => {
            const Icon = asset.icon
            return (
              <div
                key={asset.name}
                className={
                  asset.active
                    ? "rounded-xl border border-primary/30 bg-primary/10 p-3 transition-transform hover:-translate-y-0.5"
                    : "rounded-xl border border-dashed border-border bg-muted/20 p-3 transition-transform hover:-translate-y-0.5"
                }
              >
                <Icon
                  aria-hidden="true"
                  size={17}
                  className={
                    asset.active ? "text-primary" : "text-muted-foreground"
                  }
                />
                <p className="mt-3 truncate text-xs font-semibold text-foreground">
                  {asset.name}
                </p>
                <p
                  className={
                    "mt-1 text-[9px] font-semibold tracking-wider uppercase " +
                    (asset.active ? "text-primary" : "text-muted-foreground")
                  }
                >
                  {asset.state}
                </p>
              </div>
            )
          })}
        </div>

        <div className="mt-5 grid grid-cols-[1fr_auto_1fr] items-center gap-3 rounded-2xl border border-border bg-muted/30 p-4">
          <div className="text-center">
            <Cloud
              aria-hidden="true"
              size={18}
              className="mx-auto text-muted-foreground"
            />
            <p className="mt-2 text-[10px] font-semibold text-foreground">
              Full project
            </p>
            <p className="text-[9px] text-muted-foreground">object storage</p>
          </div>
          <div className="flex flex-col items-center gap-1 text-[9px] font-semibold text-primary">
            <span>hydrate</span>
            <span className="h-px w-14 bg-primary/60" />
            <span>dehydrate</span>
          </div>
          <div className="text-center">
            <FolderOpen
              aria-hidden="true"
              size={18}
              className="mx-auto text-primary"
            />
            <p className="mt-2 text-[10px] font-semibold text-foreground">
              Active set
            </p>
            <p className="text-[9px] text-muted-foreground">local disk</p>
          </div>
        </div>

        <p className="mt-4 text-center text-[10px] text-muted-foreground">
          Crab will not dehydrate a dirty file or replace it with a stale
          pointer.
        </p>
      </div>
    </Window>
  )
}

export function CiProfileVisual() {
  const stages = [
    { icon: GitBranch, label: "lazy clone", caption: "history + pointers" },
    { icon: PackageCheck, label: "hydrate", caption: "manifest paths" },
    { icon: Play, label: "test", caption: "real fixtures" },
  ]

  return (
    <Window label="pipeline #1842">
      <div
        className="p-5 sm:p-7"
        role="img"
        aria-label="A CI job lazily clones Git history, hydrates the paths listed in a reviewed manifest, and runs tests against real fixtures. A cache can be warmed before hydration."
      >
        <div className="flex flex-wrap items-center gap-2">
          <span className="flex items-center gap-2 rounded-full bg-primary/10 px-3 py-1.5 text-[10px] font-semibold text-primary">
            <Workflow size={12} /> pull request
          </span>
          <span className="text-[10px] text-muted-foreground">
            linux-x64 · test-shard-03
          </span>
          <span className="ml-auto flex items-center gap-1.5 text-[10px] text-primary">
            <span className="size-1.5 rounded-full bg-primary" /> running
          </span>
        </div>

        <div className="mt-6 grid grid-cols-[1fr_auto_1fr_auto_1fr] items-center gap-2">
          {stages.map((stage, index) => {
            const Icon = stage.icon
            return (
              <div key={stage.label} className="contents">
                <div
                  className={
                    index === 1
                      ? "min-w-0 rounded-xl border border-primary/40 bg-primary/10 p-3 text-center"
                      : "min-w-0 rounded-xl border border-border bg-muted/30 p-3 text-center"
                  }
                >
                  <Icon
                    aria-hidden="true"
                    size={17}
                    className={
                      "mx-auto " +
                      (index === 1 ? "text-primary" : "text-muted-foreground")
                    }
                  />
                  <p className="mt-2 truncate text-[10px] font-semibold text-foreground">
                    {stage.label}
                  </p>
                  <p className="mt-0.5 truncate text-[9px] text-muted-foreground">
                    {stage.caption}
                  </p>
                </div>
                {index < stages.length - 1 ? <FlowArrow /> : null}
              </div>
            )
          })}
        </div>

        <div className="mt-5 grid gap-3 sm:grid-cols-[1.15fr_0.85fr]">
          <div className="rounded-2xl border border-border bg-[#0b1120] p-4 text-slate-300 shadow-inner">
            <div className="flex items-center gap-2 border-b border-slate-800 pb-3 text-[10px] text-slate-500">
              <Terminal aria-hidden="true" size={12} /> .crab/manifests/ci.txt
            </div>
            <div className="mt-3 space-y-2 font-mono text-[10px]">
              <p>
                <span className="mr-3 text-slate-600">01</span>
                tests/fixtures/auth.bin
              </p>
              <p>
                <span className="mr-3 text-slate-600">02</span>
                models/tiny.safetensors
              </p>
              <p>
                <span className="mr-3 text-slate-600">03</span>
                snapshots/linux/**
              </p>
            </div>
          </div>
          <div className="rounded-2xl border border-border bg-muted/30 p-4">
            <div className="flex items-center gap-2 text-primary">
              <Database aria-hidden="true" size={15} />
              <span className="text-[10px] font-semibold tracking-wider uppercase">
                runner cache
              </span>
            </div>
            <p className="mt-3 text-xs font-semibold text-foreground">
              Warm before checkout
            </p>
            <p className="mt-1.5 text-[10px] leading-4 text-muted-foreground">
              crab fetch can pre-load selected content without changing
              working-tree files.
            </p>
          </div>
        </div>
      </div>
    </Window>
  )
}

export function CloudBoundaryVisual() {
  return (
    <Window label="direct-storage architecture">
      <div
        className="p-5 sm:p-7"
        role="img"
        aria-label="Inside the organization's cloud boundary, developer machines and CI runners use provider-native credentials to connect directly to the team's object storage. No Crab data server sits between them."
      >
        <div className="rounded-3xl border border-dashed border-primary/50 bg-primary/6 p-4 sm:p-5">
          <div className="flex items-center justify-between gap-3">
            <div className="flex items-center gap-2 text-primary">
              <ShieldCheck aria-hidden="true" size={17} />
              <span className="text-[10px] font-bold tracking-[0.14em] uppercase">
                Your cloud boundary
              </span>
            </div>
            <span className="text-[9px] text-muted-foreground">
              direct-storage mode
            </span>
          </div>

          <div className="mt-5 grid grid-cols-[1fr_auto_1.05fr] items-center gap-3">
            <div className="space-y-3">
              <Node icon={ServerCog} title="Developer" caption="crab binary" />
              <Node icon={Workflow} title="CI runner" caption="crab binary" />
            </div>
            <div className="flex h-full flex-col items-center justify-center gap-2">
              <span className="rounded-full border border-primary/30 bg-card px-2 py-1 text-center text-[8px] font-semibold text-primary">
                provider auth
              </span>
              <span
                aria-hidden="true"
                className="h-20 w-px bg-linear-to-b from-primary/20 via-primary to-primary/20"
              />
            </div>
            <div className="rounded-2xl border-2 border-primary/40 bg-card p-4 shadow-lg shadow-primary/10">
              <Cloud aria-hidden="true" size={22} className="text-primary" />
              <p className="mt-3 text-xs font-bold text-foreground">
                Object storage
              </p>
              <p className="mt-1 text-[9px] leading-4 text-muted-foreground">
                Git objects · refs · xorbs · shards · indexes
              </p>
              <div className="mt-3 flex flex-wrap gap-1">
                {["S3", "GCS", "Azure"].map((name) => (
                  <span
                    key={name}
                    className="rounded bg-muted px-1.5 py-1 text-[8px] font-semibold text-muted-foreground"
                  >
                    {name}
                  </span>
                ))}
              </div>
            </div>
          </div>

          <div className="mt-5 grid grid-cols-3 gap-2">
            {["IAM role", "service account", "managed identity"].map((auth) => (
              <div
                key={auth}
                className="rounded-lg border border-primary/15 bg-card/70 px-2 py-2 text-center text-[8px] font-medium text-muted-foreground"
              >
                {auth}
              </div>
            ))}
          </div>
        </div>

        <div className="mt-5 flex items-center justify-center gap-2 rounded-xl border border-border bg-muted/30 px-4 py-3 text-[10px] text-muted-foreground">
          <ShieldCheck aria-hidden="true" size={13} className="text-primary" />
          No Crab data server or database to deploy in the direct path
        </div>
      </div>
    </Window>
  )
}
