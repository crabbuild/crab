import {
  ArrowRight,
  Cloud,
  FolderTree,
  GitBranch,
  Layers,
  MailIcon,
  RefreshCw,
  Workflow,
} from "lucide-react"

import { MarketingLayout } from "@/components/marketing-layout"
import { HeroSection } from "@/components/marketing/hero-section"
import { Reveal } from "@/components/marketing/reveal"
import { Counter } from "@/components/marketing/counter"
import { FeatureCard } from "@/components/marketing/feature-card"
import { CTASection } from "@/components/marketing/cta-section"
import { DemoTabs } from "@/components/marketing/demo-tabs"
import { HeroArchitectureSvg } from "./diagrams/hero-architecture-svg"

// Unified Section Components
import { HowItWorks } from "@/components/marketing/how-it-works"
import { ComparisonSection } from "@/components/marketing/comparison-section"
import { ChunkingDiagramSvg } from "./landing-svgs"
import { TestimonialsCarousel } from "@/components/marketing/testimonials-carousel"
import { FAQSection } from "@/components/marketing/faq-section"
import { InstallTabs, InstallTabIcons } from "@/components/marketing/install-tabs"
import { createPageMetadata } from "@/lib/metadata"

export const metadata = createPageMetadata({
  title: "Crab — Git for any file at any scale",
  description:
    "Crab is a serverless Git remote storage solution powered by the Xet protocol for chunk-level deduplication. Version any file, any size, any number — straight into your S3, GCS, or Azure bucket.",
  path: "/",
  absoluteTitle: true,
})

const heroStats: ReadonlyArray<{
  end: number
  suffix: string
  label: string
  caption: string
}> = [
  {
    end: 500,
    suffix: "+",
    label: "MB/s chunking",
    caption: "SIMD-accelerated Gearhash CDC throughput on a single core.",
  },
  {
    end: 16,
    suffix: "×",
    label: "Parallel uploads",
    caption: "Concurrent xorb transfers saturate cloud bandwidth on push.",
  },
  {
    end: 3,
    suffix: "-tier",
    label: "Deduplication",
    caption: "Session, shard, and database index — chunks uploaded once.",
  },
  {
    end: 64,
    suffix: " MiB",
    label: "Target xorb size",
    caption: "Packed chunks balance Range GET cost against round trips.",
  },
]

/* Feature showcase data — 6 cards covering the full product matrix.
   Descriptions are kept ≤150 chars (Requirement 3.3). Each href resolves
   to an existing route within the web app. */
const featureCards: ReadonlyArray<{
  icon: typeof GitBranch
  title: string
  description: string
  href: string
}> = [
  {
    icon: GitBranch,
    title: "Standard Git UX",
    description:
      "git clone, add, commit, push — unmodified. Crab is a remote helper plus filter driver, so every workflow you already know just works.",
    href: "/docs",
  },
  {
    icon: Layers,
    title: "Chunk-Level Dedup",
    description:
      "Content-defined chunking with Gearhash plus a 3-tier session, shard, and database index. Identical chunks upload exactly once.",
    href: "/docs/cli",
  },
  {
    icon: Cloud,
    title: "Cloud-Native Storage",
    description:
      "Repositories live in S3, GCS, or Azure Blob. No servers to run, no databases to operate, no LFS endpoint to scale or pay for.",
    href: "/pricing",
  },
  {
    icon: FolderTree,
    title: "Virtual Filesystem",
    description:
      "Mount repositories and hydrate file chunks on demand. Browse huge trees without pulling every object into your checkout.",
    href: "/docs/cli/virtual-filesystem",
  },
  {
    icon: Workflow,
    title: "ML Pipeline Workflows",
    description:
      "Version datasets, weights, and checkpoints alongside code. Lazy checkout pulls only the chunks each pipeline stage actually needs.",
    href: "/use-cases",
  },
  {
    icon: RefreshCw,
    title: "Git LFS Compatible",
    description:
      "Keep existing LFS pointers, hooks, transfers, and file locks — or convert large-file history to Crab-managed storage when you're ready.",
    href: "/docs/cli/guides/migrating-from-lfs",
  },
]

const installTabsData = [
  {
    value: "macos",
    label: "macOS",
    icon: InstallTabIcons.macOS,
    lines: [
      { text: "# Install using Homebrew", type: "comment" as const },
      { text: "brew install crabbuild/tap/crab", type: "command" as const },
      { text: "==> Fetching crabbuild/tap/crab...", type: "output" as const },
      { text: "==> Installed crab 1.0.15", type: "output" as const },
      { text: "", type: "output" as const },
      { text: "# Initialize Crab in your repository", type: "comment" as const },
      { text: "crab init --storage-provider s3 crab://my-s3-bucket/my-repo", type: "command" as const },
      { text: "crab setup", type: "command" as const },
      { text: "Initialized Crab repository.", type: "output" as const },
    ],
    note: "Requires Homebrew. The tap formula installs crab and git-remote-crab in one step.",
  },
  {
    value: "linux",
    label: "Linux",
    icon: InstallTabIcons.Linux,
    lines: [
      { text: "# Install via curl script", type: "comment" as const },
      { text: "curl -fsSL https://crab.build/install.sh | bash", type: "command" as const },
      { text: "==> Downloading crab v1.0.15 for linux-x86_64", type: "output" as const },
      { text: "==> Created symlink: git-remote-crab -> crab", type: "output" as const },
      { text: "", type: "output" as const },
      { text: "# Initialize Crab", type: "comment" as const },
      { text: "crab init --storage-provider gcs crab://my-gcs-bucket/my-repo", type: "command" as const },
      { text: "crab setup", type: "command" as const },
      { text: "Initialized Crab repository.", type: "output" as const },
    ],
    note: "Supports x86_64 and aarch64. The installer verifies SHA256SUMS.txt before replacing the binary.",
  },
  {
    value: "windows",
    label: "Windows",
    icon: InstallTabIcons.Windows,
    lines: [
      { text: "# Install in PowerShell", type: "comment" as const },
      { text: "irm https://crab.build/install.ps1 | iex", type: "command" as const },
      { text: "==> Downloading crab v1.0.15 for windows-x86_64", type: "output" as const },
      { text: "==> Created helper: git-remote-crab.exe", type: "output" as const },
      { text: "", type: "comment" as const },
      { text: "# Initialize Crab remote", type: "comment" as const },
      { text: "crab init --storage-provider azure crab://my-azure-container/my-repo", type: "command" as const },
      { text: "crab setup", type: "command" as const },
      { text: "Initialized Crab repository.", type: "output" as const },
    ],
    note: "Supports x86_64 and ARM64. The installer adds ~/.crab/bin to the user PATH.",
  },
]

export default function LandingPage() {
  return (
    <MarketingLayout>
      {/* ── Hero: Enhanced with floating particles + shimmer headline ── */}
      <HeroSection
        badge={{
          text: "Now Open Source",
          href: "/blog/git-for-large-files-at-any-scale",
          dot: true,
        }}
        headline="Git for any file at any scale"
        subheadline={
          <>
            Crab is a serverless Git remote storage solution powered by the{" "}
            <a
              href="https://huggingface.co/docs/xet/en/index"
              target="_blank"
              rel="noopener noreferrer"
              className="text-primary underline underline-offset-2 hover:text-primary/80"
            >
              Xet protocol
            </a>{" "}
            for chunk-level deduplication — version huge binaries straight into
            your S3, GCS, or Azure bucket.
          </>
        }
        primaryCTA={{ label: "Get Started", href: "/docs/cli", icon: ArrowRight }}
        secondaryCTA={{ label: "Contact Us", href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog", icon: MailIcon }}
        animatedBackground="particles"
        headlineEffect="shimmer"
        diagram={<HeroArchitectureSvg />}
      />

      {/* ── Logo Marquee Bar ── */}
      {/* <LogoMarquee /> */}

      {/* ── Social Proof Stats: Animated counters ── */}
      <section
        id="social-proof"
        aria-labelledby="social-proof-heading"
        className="border-y border-border bg-muted/40"
      >
        <div className="mx-auto max-w-6xl px-6 py-16 md:py-20">
          <Reveal>
            <div className="text-center">
              <p className="text-xs font-semibold uppercase tracking-[0.08em] text-primary">
                Built for scale
              </p>
              <h2
                id="social-proof-heading"
                className="mt-2 text-3xl font-bold tracking-tight text-foreground md:text-4xl"
              >
                Numbers that hold up under big repositories
              </h2>
              <p className="mx-auto mt-3 max-w-2xl text-base text-muted-foreground">
                Crab is engineered for ML model weights, datasets, and game
                assets that break Git LFS. Every metric below comes from the
                shipping CLI.
              </p>
            </div>
          </Reveal>

          {/* 4 animated counters */}
          <ul
            role="list"
            className="mt-12 grid grid-cols-2 gap-4 md:grid-cols-4 md:gap-6"
          >
            {heroStats.map((stat) => (
              <li key={stat.label}>
                <Reveal>
                  <div className="rounded-card border border-border bg-card p-5 text-center shadow-card md:p-6">
                    <div className="font-heading text-3xl font-extrabold tracking-tight text-foreground md:text-4xl">
                      <Counter end={stat.end} suffix={stat.suffix} />
                    </div>
                    <div className="mt-1 text-sm font-semibold text-foreground">
                      {stat.label}
                    </div>
                    <p className="mt-2 text-xs leading-relaxed text-muted-foreground">
                      {stat.caption}
                    </p>
                  </div>
                </Reveal>
              </li>
            ))}
          </ul>
        </div>
      </section>

      {/* ── How It Works ── */}
      <HowItWorks />

      {/* ── Feature Showcase: 6 cards with micro-animations ── */}
      <section
        id="features"
        aria-labelledby="features-heading"
        className="border-t border-border bg-muted/40"
      >
        <div className="mx-auto max-w-6xl px-6 py-16 md:py-20">
          <Reveal>
            <div className="text-center">
              <p className="text-xs font-semibold uppercase tracking-[0.08em] text-primary">
                Core Features
              </p>
              <h2
                id="features-heading"
                className="mt-2 text-3xl font-bold tracking-tight text-foreground md:text-4xl"
              >
                Everything you need to version any file
              </h2>
              <p className="mx-auto mt-3 max-w-2xl text-base text-muted-foreground">
                One serverless toolchain across the CLI, Git, and cloud —
                covering large-file Git, deduplicated storage, AI
                agents, and ML pipelines.
              </p>
            </div>
          </Reveal>

          <Reveal>
            <ul
              role="list"
              className="mt-12 grid grid-cols-1 gap-6 md:grid-cols-2 lg:grid-cols-3"
            >
              {featureCards.map((feature) => (
                <li key={feature.title} className="h-full">
                  <FeatureCard
                    icon={feature.icon}
                    title={feature.title}
                    description={feature.description}
                    href={feature.href}
                    className="h-full"
                  />
                </li>
              ))}
            </ul>
          </Reveal>
        </div>
      </section>

      {/* ── Comparison Section: Crab vs Competitors sticky table ── */}
      <ComparisonSection />

      {/* ── Interactive CLI Demo: tabbed typing animation ── */}
      <section
        id="demo"
        aria-labelledby="demo-heading"
        className="border-t border-border bg-muted/40"
        style={{ scrollMarginTop: "80px" }}
      >
        <div className="mx-auto max-w-6xl px-6 py-16 md:py-20">
          <Reveal>
            <div className="text-center">
              <p className="text-xs font-semibold uppercase tracking-[0.08em] text-primary">
                Interactive Demo
              </p>
              <h2
                id="demo-heading"
                className="mt-2 text-3xl font-bold tracking-tight text-foreground md:text-4xl"
              >
                See it in action
              </h2>
              <p className="mx-auto mt-3 max-w-2xl text-base text-muted-foreground">
                Two common workflows — starting fresh or cloning an existing
                repo. Full end-to-end from init to push.
              </p>
            </div>
          </Reveal>

          <Reveal>
            <div className="mt-10">
              <DemoTabs />
            </div>
          </Reveal>
        </div>
      </section>

      {/* ── Architecture Deep-Dive: CDC & 3-Tier Dedup ── */}
      <section
        id="architecture-deep-dive"
        className="border-t border-border bg-background py-16 md:py-20"
      >
        <div className="mx-auto max-w-6xl px-6">
          <div className="grid items-center gap-12 md:grid-cols-2">
            <Reveal>
              <div>
                <p className="text-xs font-semibold uppercase tracking-[0.08em] text-primary">
                  Architecture
                </p>
                <h2 className="mt-2 text-3xl font-bold tracking-tight text-foreground md:text-4xl">
                  Content-Defined Chunking &amp; 3-Tier Dedup
                </h2>
                <p className="mt-4 text-base text-muted-foreground">
                  Files are split at natural content boundaries using Gearhash CDC.
                  Duplicate chunks are identified across three tiers — minimizing
                  storage costs even for large binary datasets.
                </p>
                <div className="mt-6 flex flex-col gap-3">
                  {[
                    {
                      c: "Class A",
                      b: "Existing",
                      t: "— already on remote, skipped entirely.",
                    },
                    {
                      c: "Class B",
                      b: "Staged",
                      t: "— in local staging, needs packing & upload.",
                    },
                    {
                      c: "Class C",
                      b: "New",
                      t: "— never seen before, staged, packed, uploaded.",
                    },
                  ].map((t) => (
                    <div
                      key={t.c}
                      className="flex items-center gap-2.5 text-sm text-muted-foreground"
                    >
                      <span className="shrink-0 rounded bg-primary-muted px-2.5 py-0.5 text-xs font-bold uppercase tracking-[0.04em] text-primary">
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
            <Reveal>
              <div className="flex items-center justify-center rounded-[14px] border border-border bg-muted/40 p-6 shadow-card">
                <ChunkingDiagramSvg />
              </div>
            </Reveal>
          </div>
        </div>
      </section>

      {/* ── Testimonials Carousel: Rotating customer success stories ── */}
      <TestimonialsCarousel />

      {/* ── FAQ Section: Expandable accordion ── */}
      <FAQSection />

      {/* ── Quick-Start Installer Tabs ── */}
      <section
        id="quick-start"
        className="border-t border-border bg-muted/40 py-16 md:py-20"
      >
        <div className="mx-auto max-w-3xl px-6">
          <Reveal>
            <div className="mb-10 text-center">
              <p className="text-xs font-semibold uppercase tracking-[0.08em] text-primary">
                Quickstart
              </p>
              <h2 className="mt-2 text-3xl font-bold tracking-tight text-foreground md:text-4xl">
                Get started in seconds
              </h2>
              <p className="mt-3 text-base text-muted-foreground">
                Install the Crab CLI for your platform and initialize your first
                serverless remote bucket.
              </p>
            </div>
          </Reveal>
          <Reveal>
            <InstallTabs tabs={installTabsData} />
          </Reveal>
        </div>
      </section>

      {/* ── Open Source & Community ── */}
      {/* <CommunitySection /> */}

      {/* ── Bottom CTA ── */}
      <CTASection
        headline="Ready to handle any file at any scale?"
        description="Install the Crab CLI in minutes and start versioning files in your own cloud bucket."
        primaryCTA={{
          label: "Get Started with CLI",
          href: "/docs/cli",
          icon: ArrowRight,
        }}
        secondaryCTA={{
          label: "Contact Us",
          href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog",
          icon: MailIcon,
        }}
        variant="accent"
      />
    </MarketingLayout>
  )
}
