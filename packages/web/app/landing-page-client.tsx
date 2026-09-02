"use client"

import Link from "next/link"
import {
  GitBranch, Layers, Cloud, Workflow,
  Cpu, Package, ShieldCheck, HardDrive, FolderTree, RefreshCw,
  ArrowRight,
} from "lucide-react"
import type { LucideIcon } from "lucide-react"
import type { ReactNode } from "react"
import { MarketingLayout } from "@/components/marketing-layout"
import { HeroDiagramSvg, ChunkingDiagramSvg, PipelineDiagramSvg } from "./landing-svgs"
import { Reveal } from "@/components/marketing/reveal"
import { Counter } from "@/components/marketing/counter"
import { TypingCode } from "@/components/marketing/typing-code"

function Section({
  id,
  alt = false,
  className = "",
  children,
}: {
  id?: string
  alt?: boolean
  className?: string
  children: ReactNode
}) {
  return (
    <section
      id={id}
      className={`py-section ${alt ? "border-y border-border bg-muted/40" : ""} ${className}`}
    >
      <div className="mx-auto max-w-[1080px] px-6">{children}</div>
    </section>
  )
}

function Tag({ center = false, children }: { center?: boolean; children: ReactNode }) {
  return (
    <p
      className={`text-xs font-semibold uppercase tracking-[0.06em] text-primary ${center ? "text-center" : ""}`}
    >
      {children}
    </p>
  )
}

function H2({ center = false, children }: { center?: boolean; children: ReactNode }) {
  return (
    <h2
      className={`mt-2 mb-3 text-[clamp(24px,3vw,36px)] font-bold leading-[1.2] tracking-[-0.02em] text-foreground ${center ? "text-center" : ""}`}
    >
      {children}
    </h2>
  )
}

function Desc({ center = false, children }: { center?: boolean; children: ReactNode }) {
  return (
    <p
      className={`text-[15px] leading-[1.7] text-muted-foreground ${center ? "mx-auto max-w-[600px] text-center" : "max-w-[540px]"}`}
    >
      {children}
    </p>
  )
}

function FeatureCard({ icon: Icon, title, desc }: { icon: LucideIcon; title: string; desc: string }) {
  return (
    <div className="rounded-card border border-border bg-card p-6 transition-colors duration-200 hover:border-primary/50">
      <div className="mb-3.5 flex h-9 w-9 items-center justify-center rounded-lg bg-primary-muted text-primary">
        <Icon size={18} strokeWidth={2} />
      </div>
      <h3 className="mb-1 text-[15px] font-semibold text-foreground">{title}</h3>
      <p className="text-[13px] leading-[1.6] text-muted-foreground">{desc}</p>
    </div>
  )
}

function DiagramBox({ children, maxWidth }: { children: ReactNode; maxWidth?: number }) {
  return (
    <div
      className="flex items-center justify-center rounded-[14px] border border-border bg-muted/40 px-5 py-7"
      style={maxWidth ? { maxWidth, margin: "36px auto 0" } : undefined}
    >
      <div className="w-full [&>svg]:mx-auto [&>svg]:h-auto [&>svg]:w-full [&>svg]:max-w-[480px]">
        {children}
      </div>
    </div>
  )
}

/* ═══════ PAGE ═══════ */
export function LandingPageClient() {
  const features = [
    { icon: GitBranch, title: "Standard Git UX", desc: "Works with unmodified Git. git clone crab://bucket/repo just works — no new commands to learn." },
    { icon: Layers, title: "Chunk-Level Dedup", desc: "Gearhash CDC splits files at content boundaries. Three-tier dedup minimizes storage across large binary datasets." },
    { icon: Cloud, title: "Cloud-Native Storage", desc: "Store repos directly in S3, GCS, or Azure. No LFS server, no database — just your existing cloud bucket." },
    { icon: FolderTree, title: "Virtual Filesystem", desc: "Mount repositories and hydrate chunks on demand. Browse large trees without pulling every byte first." },
    { icon: Workflow, title: "ML Pipeline Workflows", desc: "DVC-compatible pipeline engine with parallel DAG execution, crash recovery, and resource-aware scheduling." },
  ]

  const archCards = [
    { icon: Cpu, title: "Rust Core", desc: "Single binary, zero runtime deps. Async I/O via tokio with up to 16 concurrent uploads. SIMD-accelerated chunking." },
    { icon: Package, title: "Xorb Format", desc: "Chunks packed into ~64 MiB xorbs with run continuity. Reconstruct files with minimal Range GETs." },
    { icon: ShieldCheck, title: "Fail-Forward", desc: "All immutable data durable before any ref moves. Interrupted pushes never create dangling references." },
    { icon: HardDrive, title: "Local Cache", desc: "LRU xorb cache at ~/.cache/crab/. Hydrate-after-push resolves from cache — disk speed, not network." },
    { icon: FolderTree, title: "FUSE Mount", desc: "Mount repos as virtual filesystems. Files download only when read — per-chunk, per-byte-range on demand." },
    { icon: RefreshCw, title: "Git LFS Compat", desc: "LFS-tracked files stored alongside xorbs. Migrate from Git LFS without re-uploading. Dual pointer detection." },
  ]

  return (
    <MarketingLayout>
      <div className="bg-background text-foreground antialiased">

        {/* ── HERO ── */}
        <section
          id="hero"
          className="relative overflow-hidden border-b border-border px-6 pb-16 pt-[100px] text-center"
        >
          <div
            aria-hidden
            className="absolute inset-0 bg-[radial-gradient(ellipse_70%_50%_at_50%_-5%,var(--primary-muted)_0%,transparent_70%)]"
          />
          <div className="relative z-1 mx-auto max-w-[920px]">
            <Reveal>
              <Link
                href="/blog/git-for-large-files-at-any-scale"
                className="mb-5 inline-flex items-center gap-[7px] rounded-full border border-primary/20 bg-primary-muted px-3.5 py-1 text-xs font-semibold text-primary transition-colors hover:bg-primary-muted/70"
              >
                <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-primary" />
                Now Open Source
              </Link>
            </Reveal>
            <Reveal>
              <h1 className="mb-4 text-[clamp(32px,5vw,54px)] font-extrabold leading-[1.1] tracking-tight text-foreground">
                Git Storage Solution<br />
                <span className="text-primary">for Any Files</span>
              </h1>
            </Reveal>
            <Reveal>
              <p className="mx-auto mb-7 max-w-[560px] text-base leading-[1.65] text-muted-foreground">
                Crab is a modern remote Git solution powered by the Xet protocol for chunk-level deduplication. Handle any file, any size, any number — with lazy checkout, virtual filesystems, and ML pipeline workflows.
              </p>
            </Reveal>
            <Reveal>
              <div className="flex flex-wrap justify-center gap-2.5">
                <Link
                  href="/docs"
                  id="hero-cta"
                  className="inline-flex items-center gap-1.5 rounded-lg bg-primary px-[22px] py-2.5 text-sm font-semibold text-primary-foreground transition-colors duration-150 hover:bg-primary-hover"
                >
                  Get Started <ArrowRight size={14} />
                </Link>
              </div>
            </Reveal>
            <Reveal>
              <div className="mt-12">
                <HeroDiagramSvg />
              </div>
            </Reveal>
          </div>
        </section>

        {/* ── STATS ── */}
        <section id="stats" className="border-b border-border bg-muted/40 px-6 py-8">
          <div className="mx-auto grid max-w-[760px] grid-cols-2 gap-4 text-center md:grid-cols-4">
            {[
              { v: 500, s: "+ MB/s", l: "Chunking throughput" },
              { v: 3, s: "-tier", l: "Deduplication" },
              { v: 16, s: "×", l: "Parallel uploads" },
              { v: 64, s: " MiB", l: "Target xorb size" },
            ].map(d => (
              <div key={d.l}>
                <div className="text-[22px] font-extrabold text-foreground">
                  <Counter end={d.v} suffix={d.s} />
                </div>
                <div className="mt-0.5 text-xs text-muted-foreground">{d.l}</div>
              </div>
            ))}
          </div>
        </section>

        {/* ── FEATURES ── */}
        <Section id="features">
          <Reveal>
            <Tag center>Core Features</Tag>
            <H2 center>Everything you need to version any file at any scale</H2>
          </Reveal>
          <div className="mt-10 grid grid-cols-1 gap-3.5 md:grid-cols-2 lg:grid-cols-3">
            {features.map(f => (
              <Reveal key={f.title}>
                <FeatureCard {...f} />
              </Reveal>
            ))}
          </div>
        </Section>

        {/* ── CLI ── */}
        <Section id="cli" alt>
          <div className="grid items-center gap-14 md:grid-cols-2">
            <Reveal>
              <div>
                <Tag>Crab CLI</Tag>
                <H2>Remote Git with Xet Protocol</H2>
                <Desc>
                  Crab CLI acts as both a Git remote helper and a filter driver. It uses content-defined chunking with the Xet protocol — splitting files at natural byte boundaries and deduplicating at the chunk level.
                </Desc>
                <ul className="mt-4 list-none space-y-1 p-0">
                  {[
                    "SIMD-accelerated Gearhash CDC at 500+ MB/s",
                    "14-step push pipeline with fail-forward guarantees",
                    "Lazy checkout & FUSE virtual filesystem mount",
                    "Git LFS compatibility — migrate without re-uploading",
                    "Blake3 hashing for speed & collision resistance",
                  ].map(item => (
                    <li
                      key={item}
                      className="relative py-1 pl-[22px] text-sm leading-[1.6] text-muted-foreground before:absolute before:left-0 before:top-[9px] before:h-3 before:w-3 before:rounded-full before:border-2 before:border-primary before:bg-primary-muted before:content-['']"
                    >
                      {item}
                    </li>
                  ))}
                </ul>
              </div>
            </Reveal>
            <Reveal>
              <TypingCode lines={[
                "$ git clone crab://my-bucket/ml-models",
                "Cloning into 'ml-models'...",
                "remote: Downloading packs (3.2 GiB)...",
                "Receiving objects: 100% (1,247/1,247), done.",
                "",
                "$ git add model.safetensors  # 12 GB file",
                "CDC chunking: 12.4 GiB → 194,216 chunks",
                "Dedup: 87% chunks already exist (skipped)",
                "Staged: 25,248 new chunks (1.6 GiB)",
                "",
                "$ git push origin main",
                "Uploading: ████████████████ 100% (16× parallel)",
                "   a1b2c3d..f4e5d6c  main → main",
              ]} />
            </Reveal>
          </div>
        </Section>

        {/* ── DEDUP ── */}
        <Section id="dedup">
          <div className="grid items-center gap-14 md:grid-cols-2">
            <Reveal>
              <DiagramBox>
                <ChunkingDiagramSvg />
              </DiagramBox>
            </Reveal>
            <Reveal>
              <div>
                <Tag>Xet Protocol</Tag>
                <H2>Content-Defined Chunking &amp; 3-Tier Dedup</H2>
                <Desc>
                  Files are split at natural content boundaries using Gearhash CDC. Duplicate chunks are identified across three tiers — minimizing storage costs even for large binary datasets.
                </Desc>
                <div className="mt-4 flex flex-col gap-2.5">
                  {[
                    { c: "Class A", b: "Existing", t: "— already on remote, skipped entirely." },
                    { c: "Class B", b: "Staged", t: "— in local staging, needs packing & upload." },
                    { c: "Class C", b: "New", t: "— never seen before, staged, packed, uploaded." },
                  ].map(t => (
                    <div key={t.c} className="flex items-center gap-2.5 text-[13px] text-muted-foreground">
                      <span className="shrink-0 rounded bg-primary-muted px-2 py-0.5 text-[10px] font-bold uppercase tracking-[0.04em] text-primary">
                        {t.c}
                      </span>
                      <span>
                        <strong>{t.b}</strong> {t.t}
                      </span>
                    </div>
                  ))}
                </div>
              </div>
            </Reveal>
          </div>
        </Section>

        {/* ── WORKFLOWS ── */}
        <Section id="workflows">
          <div className="grid items-center gap-14 md:grid-cols-2">
            <div>
              <Reveal>
                <Tag>ML Workflows</Tag>
                <H2>DVC-Compatible Pipeline Engine</H2>
                <Desc>
                  Crab&apos;s workflow engine brings DVC-style stages together with parallel DAG execution, crash recovery, and resource scheduling. Migration is inventory-first and fails closed on unsupported checkpoints, artifacts, and providers until each cutover gate is verified.
                </Desc>
              </Reveal>
              <Reveal>
                <div className="mt-5 overflow-hidden rounded-card border border-border bg-card">
                  <h4 className="px-4 pt-3.5 text-[14px] font-semibold text-foreground">Crab vs DVC</h4>
                  <table className="mt-1.5 w-full border-collapse text-[12px]">
                    <thead>
                      <tr>
                        <th className="border-b border-border px-3.5 py-1.5 text-left font-medium text-muted-foreground">Capability</th>
                        <th className="border-b border-border px-3.5 py-1.5 text-left font-medium text-muted-foreground">Crab</th>
                        <th className="border-b border-border px-3.5 py-1.5 text-left font-medium text-muted-foreground">DVC</th>
                      </tr>
                    </thead>
                    <tbody>
                      {["Parallel DAG execution", "Crash recovery", "Retry with backoff", "Resource scheduling", "Chunk-level dedup", "FUSE virtual filesystem"].map((r, i, arr) => (
                        <tr key={r}>
                          <td className={`px-3.5 py-1.5 text-muted-foreground ${i < arr.length - 1 ? "border-b border-border" : ""}`}>{r}</td>
                          <td className={`px-3.5 py-1.5 text-center font-bold text-primary ${i < arr.length - 1 ? "border-b border-border" : ""}`}>✓</td>
                          <td className={`px-3.5 py-1.5 text-center text-muted-foreground ${i < arr.length - 1 ? "border-b border-border" : ""}`}>✗</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </Reveal>
            </div>
            <Reveal>
              <DiagramBox>
                <PipelineDiagramSvg />
              </DiagramBox>
            </Reveal>
          </div>
        </Section>

        {/* ── ARCHITECTURE ── */}
        <Section id="architecture">
          <Reveal>
            <Tag center>Architecture</Tag>
            <H2 center>Built for Performance</H2>
          </Reveal>
          <div className="mt-10 grid grid-cols-1 gap-3.5 md:grid-cols-2 lg:grid-cols-3">
            {archCards.map(c => (
              <Reveal key={c.title}>
                <FeatureCard {...c} />
              </Reveal>
            ))}
          </div>
        </Section>

        {/* ── CTA ── */}
        <section
          id="cta"
          className="border-t border-primary/20 bg-primary-muted px-6 py-20 text-center"
        >
          <Reveal>
            <h2 className="mb-2.5 text-[clamp(24px,3vw,34px)] font-bold tracking-[-0.02em] text-foreground">
              Ready to handle any file at any scale?
            </h2>
            <p className="mb-7 text-[15px] text-muted-foreground">
              Get started with Crab CLI in minutes.
            </p>
            <div className="flex flex-wrap justify-center gap-2.5">
              <Link
                href="/docs/cli"
                id="cta-start"
                className="inline-flex items-center gap-1.5 rounded-lg bg-primary px-[22px] py-2.5 text-sm font-semibold text-primary-foreground transition-colors duration-150 hover:bg-primary-hover"
              >
                Get Started with CLI <ArrowRight size={14} />
              </Link>
            </div>
          </Reveal>
        </section>
      </div>
    </MarketingLayout>
  )
}
