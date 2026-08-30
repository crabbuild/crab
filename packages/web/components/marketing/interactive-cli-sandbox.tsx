"use client"

import { useState } from "react"
import { PlayCircle, Database, RefreshCw, HardDrive, Terminal } from "lucide-react"

import { TypingCode } from "@/components/marketing/typing-code"
import { cn } from "@/lib/utils"

interface SandboxTab {
  id: string
  label: string
  command: string
  icon: typeof Database
  description: string
  lines: string[]
}

const SANDBOX_TABS: SandboxTab[] = [
  {
    id: "init",
    label: "Initialize Remote",
    command: "crab init + setup",
    icon: Database,
    description: "Connect your local Git repository to a serverless S3, GCS, or Azure bucket, then scan for large files and configure the filter driver.",
    lines: [
      "$ crab init crab://my-ml-datasets/my-repo",
      "# Initialized git repository in /home/user/my-repo",
      "# Registering crab filter driver in .git/config...",
      "$ crab setup",
      "# Scanning working tree for large files...",
      "# Detected large files — tracking: *.safetensors, *.bin",
      "",
      "✔ Auto-tracked 2 extension(s) in .gitattributes",
      "✔ Remote configured at .crab/local.toml",
    ],
  },
  {
    id: "push",
    label: "Push Large Files",
    command: "crab add + git push",
    icon: PlayCircle,
    description: "Stage large files with parallel chunking, then push. Chunks are automatically deduplicated and packed into xorbs.",
    lines: [
      "$ crab add '*.safetensors'  # parallel CDC chunking",
      "  Staged 1 file (12.4 GiB → 193,716 chunks)",
      "$ git commit -m \"add updated fine-tuned model weights\"",
      "$ git push crab main",
      "",
      "remote: 3-tier dedup: 89.2% chunks already in bucket (skipped)",
      "remote: Packing 20,921 new chunks → 21 xorbs (1.34 GiB)",
      "remote: Uploading xorbs: ████████████████ 100% (16× parallel)",
      "remote: Ref advanced via CAS (etag=2f3a8b)",
      "   abc1234..def5678  main → main",
    ],
  },
  {
    id: "status",
    label: "Status & Cache",
    command: "crab status",
    icon: RefreshCw,
    description: "Check which files are hydrated locally or remain as lightweight pointers to save disk space.",
    lines: [
      "$ crab status",
      "Remote: crab://my-ml-datasets/my-repo",
      "",
      "Files tracked (4 files, 20.8 GiB total):",
      "  [hydrated] models/config.json           2.1 KiB",
      "  [hydrated] models/tokenizer.json        485 KiB",
      "  [pointer]  models/llama-weights.safetensors  12.4 GiB",
      "  [pointer]  datasets/train.parquet       8.4 GiB",
      "",
      "# Run `crab hydrate` to materialize pointer files.",
    ],
  },
  {
    id: "mount",
    label: "FUSE Virtual Mount",
    command: "crab mount",
    icon: HardDrive,
    description: "Mount a repository as a virtual filesystem. Files are fetched from object storage only when read — no full download required.",
    lines: [
      "$ crab mount --repo crab://bucket/datasets --mountpoint ~/mnt/data",
      "# Connecting to coordinator...",
      "# FUSE mount active at: /Users/user/mnt/data",
      "# Refresh loop: polling remote every 30s",
      "",
      "# In another terminal:",
      "$ ls ~/mnt/data/",
      "  train.parquet  val.parquet  config.json",
      "$ head -c 4096 ~/mnt/data/train.parquet",
      "  Fetched 1 chunk (4 KiB) from S3 in 45ms",
    ],
  },
]

export function InteractiveCliSandbox() {
  const [activeTab, setActiveTab] = useState(SANDBOX_TABS[0])

  return (
    <div className="w-full">
      <div className="grid gap-8 lg:grid-cols-12">
        {/* Left Side: Dynamic controls & prose */}
        <div className="lg:col-span-5 flex flex-col justify-center">
          <p className="text-xs font-semibold uppercase tracking-[0.08em] text-primary mb-2">
            Interactive Shell Sandbox
          </p>
          <h3 className="text-2xl font-bold tracking-tight text-foreground md:text-3xl">
            Experience the Crab CLI Workflow
          </h3>
          <p className="mt-3 text-sm leading-relaxed text-muted-foreground">
            Select a common developer command to see how the single Rust binary chunk-deduplicates, parallel-uploads, and lazy-mounts files straight to your cloud store.
          </p>

          <nav className="mt-6 flex flex-col gap-2" aria-label="CLI commands navigation">
            {SANDBOX_TABS.map((tab) => {
              const TabIcon = tab.icon
              const isActive = tab.id === activeTab.id

              return (
                <button
                  key={tab.id}
                  type="button"
                  onClick={() => setActiveTab(tab)}
                  className={cn(
                    "flex items-center gap-3.5 rounded-xl border p-4 text-left transition-all duration-(--duration-normal) ease-(--ease-out-app)",
                    isActive
                      ? "border-primary bg-primary-muted/50 shadow-card"
                      : "border-border bg-card/60 hover:bg-card hover:shadow-card-hover"
                  )}
                >
                  <span
                    className={cn(
                      "flex h-9 w-9 items-center justify-center rounded-lg transition-colors",
                      isActive
                        ? "bg-primary text-primary-foreground"
                        : "bg-muted text-muted-foreground group-hover:text-foreground"
                    )}
                  >
                    <TabIcon size={16} strokeWidth={2} />
                  </span>
                  <div className="min-w-0 flex-1">
                    <span className="block text-sm font-semibold text-foreground">
                      {tab.label}
                    </span>
                    <span className="block font-mono text-[10px] text-muted-foreground mt-0.5">
                      {tab.command}
                    </span>
                  </div>
                </button>
              )
            })}
          </nav>
        </div>

        {/* Right Side: Animated Code Output Terminal */}
        <div className="lg:col-span-7 flex flex-col justify-center">
          <div className="relative">
            {/* Ambient backdrop glow */}
            <div
              aria-hidden="true"
              className="absolute -inset-1 rounded-card bg-gradient-to-tr from-primary to-accent opacity-20 blur-lg"
            />
            <div className="relative">
              {/* Dynamic typing session */}
              <TypingCode
                key={activeTab.id} // Forces re-mount and triggers new animation on tab switch
                lines={activeTab.lines}
                charDelay={12}
                charJitter={6}
                lineDelay={300}
                title={`Terminal — ${activeTab.command}`}
              />
            </div>
          </div>
          {/* Active Tab Explanatory description card */}
          <div className="mt-4 rounded-xl border border-border bg-card/60 p-4 backdrop-blur-md">
            <span className="inline-flex items-center gap-1.5 font-mono text-[10px] font-bold uppercase tracking-wider text-primary">
              <Terminal size={11} /> Details
            </span>
            <p className="mt-1.5 text-xs text-muted-foreground">
              {activeTab.description}
            </p>
          </div>
        </div>
      </div>
    </div>
  )
}
