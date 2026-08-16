import type { Metadata } from "next"
import {
  Brain,
  Database,
  Gamepad2,
  HardDrive,
  Building2,
  GitBranch,
  BookOpen,
  ArrowRight,
  CreditCard,
  Layers,
  Lock,
  Package,
  Rocket,
  ShieldCheck,
  Timer,
  Workflow,
  type LucideIcon,
  MailIcon,
} from "lucide-react"

import { MarketingLayout } from "@/components/marketing-layout"
import { HeroSection } from "@/components/marketing/hero-section"
import { FeatureCard } from "@/components/marketing/feature-card"
import { DiagramBox } from "@/components/marketing/diagram-box"
import { CTASection } from "@/components/marketing/cta-section"
import { ComparisonTable } from "@/components/marketing/comparison-table"
import { Reveal } from "@/components/marketing/reveal"
import { UseCaseWorkflowSvg } from "@/app/diagrams/use-case-workflow-svg"
import { BeforeAfterWorkflowSvg } from "@/app/diagrams/before-after-workflow-svg"
import { ParquetAppendSvg } from "@/app/diagrams/parquet-append-svg"
import { LazyHydrateSvg } from "@/app/diagrams/lazy-hydrate-svg"
import { EnterpriseArchitectureSvg } from "@/app/diagrams/enterprise-architecture-svg"
import { CiTimelineSvg } from "@/app/diagrams/ci-timeline-svg"
import { cn } from "@/lib/utils"

export const metadata: Metadata = {
  title: "Use Cases — Crab",
  description:
    "How teams use Crab for ML/AI, data science, game development, large binary assets, enterprise, and DevOps/CI-CD workflows.",
  openGraph: {
    title: "Use Cases — Crab",
    description:
      "How teams use Crab for ML/AI, data science, game development, large binary assets, enterprise, and DevOps/CI-CD workflows.",
  },
}

// ---------- Local helpers --------------------------------------------------

interface Benefit {
  /** Numeric headline, e.g. "70%", "10×", "$0". */
  value: string
  /** Short caption describing what the number measures. */
  label: string
}

function BenefitGrid({ benefits }: { benefits: Benefit[] }) {
  return (
    <dl
      className={cn(
        "mt-8 grid gap-4 rounded-lg border border-border bg-card p-6",
        "sm:grid-cols-2",
        benefits.length >= 3 && "lg:grid-cols-3",
      )}
    >
      {benefits.map((benefit) => (
        <div key={benefit.label} className="flex flex-col gap-1">
          <dt className="text-3xl font-bold tracking-tight text-primary">
            {benefit.value}
          </dt>
          <dd className="text-sm text-muted-foreground">{benefit.label}</dd>
        </div>
      ))}
    </dl>
  )
}

interface ScenarioHeaderProps {
  icon: LucideIcon
  eyebrow: string
  title: string
  tagline: string
}

function ScenarioHeader({
  icon: Icon,
  eyebrow,
  title,
  tagline,
}: ScenarioHeaderProps) {
  return (
    <div className="space-y-3">
      <div className="flex items-center gap-3">
        <div
          aria-hidden="true"
          className="inline-flex h-10 w-10 items-center justify-center rounded-md bg-primary/10 text-primary"
        >
          <Icon size={20} strokeWidth={2} />
        </div>
        <span className="text-xs font-semibold uppercase tracking-wider text-primary">
          {eyebrow}
        </span>
      </div>
      <h2 className="text-3xl font-bold tracking-tight text-foreground md:text-4xl">
        {title}
      </h2>
      <p className="max-w-2xl text-lg text-muted-foreground">{tagline}</p>
    </div>
  )
}

// ---------- Scenario navigation -------------------------------------------

const scenarioNavItems: Array<{ id: string; label: string }> = [
  { id: "ml-ai", label: "ML & AI" },
  { id: "data-science", label: "Data Science" },
  { id: "game-dev", label: "Game Dev" },
  { id: "large-binary", label: "Large Binary" },
  { id: "enterprise", label: "Enterprise" },
  { id: "devops-cicd", label: "DevOps & CI/CD" },
]

function ScenarioNav() {
  return (
    <nav
      aria-label="Scenarios"
      className={cn(
        "sticky top-16 z-30 border-b border-border bg-background/85 backdrop-blur",
        "supports-backdrop-filter:bg-background/70",
      )}
    >
      <div className="mx-auto max-w-6xl px-6">
        <ul
          className={cn(
            "flex items-center gap-1 overflow-x-auto py-3",
            "scrollbar-thin",
          )}
        >
          {scenarioNavItems.map((item) => (
            <li key={item.id} className="shrink-0">
              <a
                href={`#${item.id}`}
                className={cn(
                  "inline-flex items-center rounded-md px-3 py-1.5 text-sm font-medium",
                  "text-muted-foreground transition-colors",
                  "hover:bg-muted hover:text-foreground",
                  "focus-visible:outline-none focus-visible:ring-2",
                  "focus-visible:ring-ring focus-visible:ring-offset-2",
                  "focus-visible:ring-offset-background",
                )}
              >
                {item.label}
              </a>
            </li>
          ))}
        </ul>
      </div>
    </nav>
  )
}

// ---------- Comparison data ------------------------------------------------

const comparisonData: {
  headers: string[]
  rows: Array<{ label: string; values: Array<boolean | string> }>
} = {
  headers: ["Crab", "Git LFS", "DVC", "Hugging Face Hub"],
  rows: [
    {
      label: "Maximum file size",
      values: ["Unlimited", "5 GB (GitHub)", "Unlimited", "50 GB per file"],
    },
    {
      label: "Deduplication method",
      values: [
        "Content-defined chunking + 3-tier dedup",
        "None (whole-file)",
        "Whole-file content hashing",
        "Xet chunk-level dedup (newer repos)",
      ],
    },
    {
      label: "Server infrastructure required",
      values: [
        "None — object storage only",
        "LFS server (self-hosted or SaaS)",
        "None — object storage only",
        "Hugging Face hosted SaaS",
      ],
    },
    {
      label: "Supported storage backends",
      values: [
        "S3, GCS, Azure Blob (any object store)",
        "Git host's LFS service",
        "S3, GCS, Azure, SSH, local",
        "Hugging Face Hub only",
      ],
    },
    {
      label: "Lazy / partial checkout",
      values: [
        "Yes — pointer blobs + FUSE mount",
        "No — full file on checkout",
        "Partial via dvc pull <path>",
        "Yes — hf hub download per file",
      ],
    },
    {
      label: "Git compatibility level",
      values: [
        "Native — git remote helper",
        "Native — git extension",
        "Sidecar — separate dvc CLI",
        "Git-compatible mirror (LFS/Xet)",
      ],
    },
    {
      label: "Cloud-native auth (IAM/SA/MI)",
      values: [true, false, true, false],
    },
    {
      label: "No SaaS dependency",
      values: [true, false, true, false],
    },
  ],
}

// ---------- Page ----------------------------------------------------------

export default function UseCasesPage() {
  return (
    <MarketingLayout>
      {/* Hero */}
      <HeroSection
        badge={{ text: "Use Cases", dot: true }}
        headline="One tool for every large-file workflow"
        subheadline="From multi-gigabyte model weights to game asset libraries to CI fixtures, Crab fits anywhere teams already use Git. No servers, no LFS endpoints — just your cloud bucket."
        primaryCTA={{
          label: "Read the Docs",
          href: "/docs/cli",
          icon: BookOpen,
        }}
        secondaryCTA={{
          label: "Contact Us",
          href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog",
          icon: MailIcon,
        }}
      />

      <ScenarioNav />

      {/* ML & AI Teams */}
      <section
        id="ml-ai"
        aria-labelledby="ml-ai-heading"
        className="scroll-mt-24 bg-background py-16 md:py-24"
      >
        <div className="mx-auto max-w-6xl px-6">
          <Reveal>
            <ScenarioHeader
              icon={Brain}
              eyebrow="ML & AI Teams"
              title="Version a 70 GB checkpoint like it's source code"
              tagline="Track model weights, datasets, and adapters with the same git workflow that ships your training code — and re-upload only the bytes that actually changed."
            />
            <h2 id="ml-ai-heading" className="sr-only">
              ML and AI Teams
            </h2>

            <div className="mt-6 max-w-3xl space-y-4 text-base text-muted-foreground md:text-lg">
              <p>
                <strong className="text-foreground">The problem.</strong>{" "}
                Every fine-tune produces a fresh multi-gigabyte checkpoint.
                Git LFS uploads the whole file every time. Custom S3 scripts
                drift away from the commit graph, so reproducing an experiment
                six months later means reverse-engineering filenames in a
                shared bucket.
              </p>
              <p>
                <strong className="text-foreground">The Crab solution.</strong>{" "}
                Content-defined chunking splits weights into variable-sized
                blocks, so a fine-tune that touches a few transformer layers
                re-uploads only the changed chunks. The 3-tier dedup pipeline
                (session → shard → DB index) shares chunks across branches
                and forks. Lazy checkout lets researchers clone a 200 GB
                experiment repo in seconds and hydrate only the artifacts
                their evaluation needs.
              </p>
            </div>

            <BenefitGrid
              benefits={[
                { value: "70%", label: "Storage savings on incremental fine-tunes" },
                { value: "10×", label: "Faster clone vs. Git LFS for multi-GB repos" },
                { value: "$0", label: "SaaS fees — pay only your cloud storage" },
              ]}
            />
          </Reveal>

          <Reveal>
            <div className="mt-12">
              <DiagramBox maxWidth={820}>
                <UseCaseWorkflowSvg />
              </DiagramBox>
            </div>
          </Reveal>

          <Reveal>
            <div className="mt-10 grid gap-6 sm:grid-cols-2 lg:grid-cols-3">
              <FeatureCard
                icon={Brain}
                title="Model versioning"
                description="Branch, tag, and roll back model weights with the same Git workflow you already use for code. git checkout v0.4.2 — get the whole stack."
              />
              <FeatureCard
                icon={Database}
                title="Dataset pinning"
                description="Lock experiments to immutable dataset commits. Reproduce any run by checking out one ref — no separate data registry to keep in sync."
              />
              <FeatureCard
                icon={Layers}
                title="Chunk-level reuse"
                description="A 1 GB checkpoint that shares 95% of its bytes with the previous epoch uploads ~50 MB. LoRA adapters cost almost nothing to ship."
              />
            </div>
          </Reveal>
        </div>
      </section>

      {/* Data Science */}
      <section
        id="data-science"
        aria-labelledby="data-science-heading"
        className="scroll-mt-24 bg-muted/40 py-16 md:py-24"
      >
        <div className="mx-auto max-w-6xl px-6">
          <Reveal>
            <ScenarioHeader
              icon={Database}
              eyebrow="Data Science"
              title="One commit per dataset, forever reproducible"
              tagline="Stop emailing S3 keys. Datasets live in the same repo as the notebook, with chunk-level dedup that handles append-only growth efficiently."
            />
            <h2 id="data-science-heading" className="sr-only">
              Data Science
            </h2>

            <div className="mt-6 max-w-3xl space-y-4 text-base text-muted-foreground md:text-lg">
              <p>
                <strong className="text-foreground">The problem.</strong>{" "}
                Notebooks reference Parquet, CSV, and feather files that
                change week-over-week. Teams email links to S3 keys, copy
                data into personal scratch buckets, and spend hours diffing
                &ldquo;v3 final FINAL&rdquo; folders. Reproducibility breaks
                the moment a colleague overwrites a path.
              </p>
              <p>
                <strong className="text-foreground">The Crab solution.</strong>{" "}
                Datasets and analysis sit in the same Git repo. Content-defined
                chunking handles append-only growth — adding a million rows
                to a 50 GB Parquet file uploads only the new tail chunks.
                3-tier dedup shares chunks across feature-engineering branches,
                so exploring 10 variants doesn&apos;t cost 10× the storage.
                Every notebook is pinned to an exact dataset commit.
              </p>
            </div>

            <BenefitGrid
              benefits={[
                { value: "85%", label: "Storage savings on append-only Parquet datasets" },
                { value: "60%", label: "Less time reconciling data versions across teammates" },
                { value: "1:1", label: "Notebook-to-dataset commit traceability" },
              ]}
            />
          </Reveal>

          <Reveal>
            <div className="mt-12">
              <DiagramBox maxWidth={820}>
                <ParquetAppendSvg />
              </DiagramBox>
            </div>
          </Reveal>

          <Reveal>
            <div className="mt-10 grid gap-6 sm:grid-cols-2 lg:grid-cols-3">
              <FeatureCard
                icon={Database}
                title="Dataset versioning"
                description="Roll back to any prior dataset revision with git checkout. No external metadata store. No drift between code and data."
              />
              <FeatureCard
                icon={HardDrive}
                title="Shared datasets"
                description="git clone crab://bucket/datasets and hydrate on demand — no copying gigabytes across machines just to read one column."
              />
              <FeatureCard
                icon={Workflow}
                title="Reproducible experiments"
                description="Every analysis links to an immutable dataset commit. Re-run a notebook six months later and get identical results."
              />
            </div>
          </Reveal>
        </div>
      </section>

      {/* Game Development */}
      <section
        id="game-dev"
        aria-labelledby="game-dev-heading"
        className="scroll-mt-24 bg-background py-16 md:py-24"
      >
        <div className="mx-auto max-w-6xl px-6">
          <Reveal>
            <ScenarioHeader
              icon={Gamepad2}
              eyebrow="Game Development"
              title="500 GB project, 30-second clone"
              tagline="Texture atlases, audio banks, FBX meshes, and baked lighting all live in Git. Re-exports upload deltas. The editor opens before the disk fills."
            />
            <h2 id="game-dev-heading" className="sr-only">
              Game Development
            </h2>

            <div className="mt-6 max-w-3xl space-y-4 text-base text-muted-foreground md:text-lg">
              <p>
                <strong className="text-foreground">The problem.</strong>{" "}
                Game projects accumulate hundreds of gigabytes of binary
                assets. Re-exporting a single texture re-uploads the whole
                file under Git LFS. A fresh clone takes hours, fills the
                disk, and the artist still hasn&apos;t opened the editor.
              </p>
              <p>
                <strong className="text-foreground">The Crab solution.</strong>{" "}
                Content-defined chunking detects that a re-exported texture
                shares most of its bytes with the previous version and uploads
                only the deltas. The optional FUSE mount presents the full
                asset tree as a virtual filesystem — the editor opens
                immediately and chunks stream in on first read. History stays
                cheap because every duplicate chunk across the entire project
                is stored once.
              </p>
            </div>

            <BenefitGrid
              benefits={[
                { value: "80%", label: "Reduction in upload size on iterative asset re-exports" },
                { value: "50×", label: "Faster initial clone via FUSE-backed lazy checkout" },
                { value: "90%", label: "Cloud storage savings on multi-year asset history" },
              ]}
            />
          </Reveal>

          <Reveal>
            <div className="mt-12">
              <DiagramBox maxWidth={820}>
                <BeforeAfterWorkflowSvg />
              </DiagramBox>
            </div>
          </Reveal>

          <Reveal>
            <div className="mt-10 grid gap-6 sm:grid-cols-2 lg:grid-cols-3">
              <FeatureCard
                icon={Gamepad2}
                title="Asset deduplication"
                description="Texture atlases, audio banks, and meshes are chunked. Re-exports upload only the changed bytes — not the whole asset."
              />
              <FeatureCard
                icon={HardDrive}
                title="Full history, low cost"
                description="Keep every revision of every asset in Git without ballooning your S3 bill. Old chunks survive once, not per-revision."
              />
              <FeatureCard
                icon={Layers}
                title="FUSE mount"
                description="Browse a 500 GB project as if it were on disk. Hydrate only the chunks the editor opens. No more 'wait for clone' Slack pings."
              />
            </div>
          </Reveal>
        </div>
      </section>

      {/* Large Binary Assets */}
      <section
        id="large-binary"
        aria-labelledby="large-binary-heading"
        className="scroll-mt-24 bg-muted/40 py-16 md:py-24"
      >
        <div className="mx-auto max-w-6xl px-6">
          <Reveal>
            <ScenarioHeader
              icon={HardDrive}
              eyebrow="Large Binary Assets"
              title="No per-file ceiling. No SaaS in the middle."
              tagline="Scientific archives, medical imaging, geospatial tiles, CAD assemblies — store any file at any size, byte-identical, directly in object storage."
            />
            <h2 id="large-binary-heading" className="sr-only">
              Large Binary Assets
            </h2>

            <div className="mt-6 max-w-3xl space-y-4 text-base text-muted-foreground md:text-lg">
              <p>
                <strong className="text-foreground">The problem.</strong>{" "}
                Scientific archives, medical imaging, geospatial tiles, and
                CAD assemblies push individual files into the tens of
                gigabytes. Git LFS chokes above 5 GB per file, requires a
                managed server, and never deduplicates within a single blob.
                Plain S3 uploads lose all version history and branching.
              </p>
              <p>
                <strong className="text-foreground">The Crab solution.</strong>{" "}
                Crab has no per-file size ceiling — files are stored as xorbs
                (compressed chunk packs) directly in object storage. Resumable
                uploads survive flaky connections. Lazy checkout lets a
                workstation pull only the slices of a 200 GB volume that a
                given task needs. Verification is byte-identical via Blake3
                hashes, end-to-end.
              </p>
            </div>

            <BenefitGrid
              benefits={[
                { value: "∞", label: "No per-file size cap (vs. 5 GB on Git LFS)" },
                { value: "75%", label: "Cost ratio vs. duplicated full-file storage" },
                { value: "100%", label: "Byte-identical reconstruction, verified by Blake3" },
              ]}
            />
          </Reveal>

          <Reveal>
            <div className="mt-12">
              <DiagramBox maxWidth={820}>
                <LazyHydrateSvg />
              </DiagramBox>
            </div>
          </Reveal>

          <Reveal>
            <div className="mt-10">
              <ComparisonTable
                title="Crab vs. Git LFS vs. DVC vs. Hugging Face Hub"
                headers={comparisonData.headers}
                rows={comparisonData.rows}
              />
            </div>
          </Reveal>
        </div>
      </section>

      {/* Enterprise */}
      <section
        id="enterprise"
        aria-labelledby="enterprise-heading"
        className="scroll-mt-24 bg-background py-16 md:py-24"
      >
        <div className="mx-auto max-w-6xl px-6">
          <Reveal>
            <ScenarioHeader
              icon={Building2}
              eyebrow="Enterprise"
              title="Zero new vendors. Your VPC, your IAM, your bucket."
              tagline="Crab is a single binary that talks to object storage with the credentials your team already manages. No SaaS, no separate control plane, nothing for security review to gate."
            />
            <h2 id="enterprise-heading" className="sr-only">
              Enterprise
            </h2>

            <div className="mt-6 max-w-3xl space-y-4 text-base text-muted-foreground md:text-lg">
              <p>
                <strong className="text-foreground">The problem.</strong>{" "}
                Security and platform teams resist adding another SaaS vendor
                to the supply chain. Existing LFS solutions require
                provisioning servers, managing certificates, rotating
                credentials, and re-running SOC&nbsp;2 review for yet another
                hosted service — all to push a few gigabytes through a
                bucket the organization already owns.
              </p>
              <p>
                <strong className="text-foreground">The Crab solution.</strong>{" "}
                Crab is a single binary with cloud-native authentication —
                IAM roles on AWS, service accounts on GCP, managed identities
                on Azure. There is no Crab server, no separate control plane,
                and no data egress outside your VPC. The 3-tier deduplication
                runs entirely on the developer&apos;s machine and the bucket;
                audit logs flow through the cloud provider&apos;s existing
                tooling.
              </p>
            </div>

            <BenefitGrid
              benefits={[
                { value: "0", label: "New servers, services, or SaaS vendors to onboard" },
                { value: "100%", label: "Of data stays inside your existing VPC and IAM perimeter" },
                { value: "1", label: "Binary to deploy across developer machines and CI runners" },
              ]}
            />
          </Reveal>

          <Reveal>
            <div className="mt-12">
              <DiagramBox maxWidth={820}>
                <EnterpriseArchitectureSvg />
              </DiagramBox>
            </div>
          </Reveal>

          <Reveal>
            <div className="mt-10 grid gap-6 sm:grid-cols-2 lg:grid-cols-3">
              <FeatureCard
                icon={ShieldCheck}
                title="Cloud-native auth"
                description="IAM roles, service accounts, and managed identities. Reuse the security posture you already audit — no new credentials to rotate."
              />
              <FeatureCard
                icon={Package}
                title="Single binary"
                description="One static binary on developer machines and CI runners. No control plane to deploy, scale, or patch."
              />
              <FeatureCard
                icon={Lock}
                title="Zero attack surface"
                description="No third-party servers, no proxy, no data egress outside your VPC. The bucket you already own is the entire dependency."
              />
            </div>
          </Reveal>
        </div>
      </section>

      {/* DevOps / CI-CD */}
      <section
        id="devops-cicd"
        aria-labelledby="devops-cicd-heading"
        className="scroll-mt-24 bg-muted/40 py-16 md:py-24"
      >
        <div className="mx-auto max-w-6xl px-6">
          <Reveal>
            <ScenarioHeader
              icon={GitBranch}
              eyebrow="DevOps & CI/CD"
              title="Cut CI clone time by 90%, on every PR"
              tagline="Lazy checkout pulls only the fixtures a job actually reads. Runner-local chunk cache means repeat builds skip the network entirely."
            />
            <h2 id="devops-cicd-heading" className="sr-only">
              DevOps and CI/CD
            </h2>

            <div className="mt-6 max-w-3xl space-y-4 text-base text-muted-foreground md:text-lg">
              <p>
                <strong className="text-foreground">The problem.</strong>{" "}
                CI runners spend most of their wall-clock time cloning repos
                that contain trained models, signed artifacts, container base
                layers, or test fixtures measured in gigabytes. Cache misses
                on a self-hosted runner translate directly into minutes of
                paid compute and slower feedback on every PR.
              </p>
              <p>
                <strong className="text-foreground">The Crab solution.</strong>{" "}
                CI jobs use lazy checkout to clone only pointer blobs and
                hydrate on demand — a test that needs one fixture pulls one
                fixture, not the whole 80 GB suite. The chunk cache on a
                runner is reused across jobs, so subsequent builds hit local
                disk instead of S3. Resumable uploads mean a flaky runner
                doesn&apos;t restart a 5 GB push from scratch.
              </p>
            </div>

            <BenefitGrid
              benefits={[
                { value: "90%", label: "Reduction in CI clone time on large-asset repos" },
                { value: "65%", label: "Lower compute cost per pipeline run" },
                { value: "0", label: "Wasted bytes from re-pushing after a transient failure" },
              ]}
            />
          </Reveal>

          <Reveal>
            <div className="mt-12">
              <DiagramBox maxWidth={820}>
                <CiTimelineSvg />
              </DiagramBox>
            </div>
          </Reveal>

          <Reveal>
            <div className="mt-10 grid gap-6 sm:grid-cols-2 lg:grid-cols-3">
              <FeatureCard
                icon={Timer}
                title="Lazy checkout in CI"
                description="Pull only the artifacts a job actually reads. A 100 GB repo behaves like a 100 MB clone — and feedback lands faster."
              />
              <FeatureCard
                icon={Layers}
                title="Runner-local chunk cache"
                description="Warm cache across pipeline runs. Repeat builds hit local disk instead of object storage. Same fixtures, near-zero pull time."
              />
              <FeatureCard
                icon={Rocket}
                title="Resumable uploads"
                description="Network blips don't restart a multi-gigabyte push. Resume from the last completed xorb — flaky runners stop costing money."
              />
            </div>
          </Reveal>
        </div>
      </section>

      {/* CTA */}
      <CTASection
        headline="Ready to ship large files like source code?"
        description="Pick the plan that fits, or jump straight into the CLI guide."
        primaryCTA={{
          label: "View Pricing",
          href: "/pricing",
          icon: CreditCard,
        }}
        secondaryCTA={{
          label: "Contact Us",
          href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog",
          icon: MailIcon,
        }}
      />
    </MarketingLayout>
  )
}
