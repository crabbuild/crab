"use client"

import { useState } from "react"
import { TypingCode, type TypingLine } from "./typing-code"
import { cn } from "@/lib/utils"

const newRepoLines: TypingLine[] = [
  { text: "# Initialize a new git repository", type: "comment" },
  { text: "mkdir ml-models && cd ml-models", type: "command" },
  { text: "git init", type: "command" },
  { text: "Initialized empty Git repository in ./ml-models/.git/", type: "output" },
  { text: "", type: "output" },
  { text: "# Install Crab and set up the remote", type: "comment" },
  { text: "crab init --storage-provider s3 crab://my-bucket/ml-models", type: "command" },
  { text: "Configured remote 'origin' → crab://my-bucket/ml-models", type: "output" },
  { text: "Installed clean/smudge filter driver", type: "output" },
  { text: "crab setup", type: "command" },
  { text: "Auto-tracked large-file patterns in .gitattributes", type: "output" },
  { text: "", type: "output" },
  { text: "# Add a large model file (13 GB)", type: "comment" },
  { text: "crab add weights/checkpoint-7b.safetensors", type: "command" },
  { text: "CDC chunking 13.4 GiB @ 512 MB/s ......... done", type: "output" },
  { text: "  chunks:    21,408   avg 656 KiB", type: "output" },
  { text: "  dedup:     0 hit / 21,408 new  (first push)", type: "output" },
  { text: "  staged:    21,408 chunks packed into 12 xorbs (13.4 GiB)", type: "output" },
  { text: "", type: "output" },
  { text: "# Commit and push to cloud storage", type: "comment" },
  { text: "git commit -m \"Add 7B checkpoint weights\"", type: "command" },
  { text: "[main (root-commit) a1b2c3d] Add 7B checkpoint weights", type: "output" },
  { text: "", type: "output" },
  { text: "git push origin main", type: "command" },
  { text: "Uploading 12 xorbs in parallel (16× concurrency)", type: "output" },
  { text: "  ████████████████████████████████ 100%  13.4 GiB", type: "output" },
  { text: "Pushed 13.4 GiB — shard + file-index uploaded", type: "output" },
  { text: "main -> main (ref CAS ok)", type: "output" },
]

const cloneRepoLines: TypingLine[] = [
  { text: "# Clone an existing Crab repository", type: "comment" },
  { text: "git clone crab://acme-team/ml-models ./ml-models", type: "command" },
  { text: "Cloning into './ml-models'...", type: "output" },
  { text: "Resolving manifest from s3://acme-team/ml-models", type: "output" },
  { text: "Fetched 12 refs, 4 packs (2.1 MiB)", type: "output" },
  { text: "Checkout complete — 248 pointer blobs (lazy)", type: "output" },
  { text: "", type: "output" },
  { text: "# Hydrate only the files you need", type: "comment" },
  { text: "crab hydrate 'weights/*.safetensors'", type: "command" },
  { text: "Resolving 3 files → 64,224 chunks across 48 xorbs", type: "output" },
  { text: "Downloading from s3://acme-team/ml-models/xorbs/", type: "output" },
  { text: "  ████████████████████████████████ 100%  38.2 GiB", type: "output" },
  { text: "Hydrated 3 files (38.2 GiB) — verified via blake3", type: "output" },
  { text: "", type: "output" },
  { text: "# Make changes and push back (dedup kicks in)", type: "comment" },
  { text: "crab add weights/checkpoint-7b-v2.safetensors", type: "command" },
  { text: "CDC chunking 13.4 GiB @ 512 MB/s ......... done", type: "output" },
  { text: "  chunks:    21,408   avg 656 KiB", type: "output" },
  { text: "  dedup:     14,902 hit / 6,506 new  (69.6%)", type: "output" },
  { text: "  staged:    new chunks packed into 7 xorbs (412 MiB)", type: "output" },
  { text: "", type: "output" },
  { text: "git commit -m \"Add v2 checkpoint\" && git push origin main", type: "command" },
  { text: "Uploading 7 xorbs in parallel (16× concurrency)", type: "output" },
  { text: "Pushed 412 MiB of 13.4 GiB — 96.9% deduplicated", type: "output" },
  { text: "main -> main (ref CAS ok)", type: "output" },
]

const tabs = [
  { id: "new", label: "New Repository", lines: newRepoLines },
  { id: "clone", label: "Clone Existing", lines: cloneRepoLines },
] as const

export function DemoTabs() {
  const [activeTab, setActiveTab] = useState<string>("new")
  const active = tabs.find((t) => t.id === activeTab) ?? tabs[0]

  return (
    <div className="mx-auto max-w-3xl">
      {/* Tab buttons */}
      <div className="mb-4 flex gap-1 rounded-lg border border-border bg-muted/60 p-1">
        {tabs.map((tab) => (
          <button
            key={tab.id}
            onClick={() => setActiveTab(tab.id)}
            className={cn(
              "flex-1 rounded-md px-4 py-2 text-sm font-medium transition-colors duration-150",
              activeTab === tab.id
                ? "bg-card text-foreground shadow-sm"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            {tab.label}
          </button>
        ))}
      </div>

      {/* Terminal */}
      <TypingCode
        key={activeTab}
        title={active.label}
        lines={[...active.lines]}
        threshold={0.3}
      />
    </div>
  )
}
