import type { Metadata } from "next"
import {
  Cloud,
  Globe,
  Upload,
  Download,
  GitBranch,
  BookOpen,
  Server,
  ArrowRightLeft,
  FileCheck,
  Layers,
  Package,
  MailIcon,
} from "lucide-react"

import { MarketingLayout } from "@/components/marketing-layout"
import { HeroSection } from "@/components/marketing/hero-section"
import { FeatureCard } from "@/components/marketing/feature-card"
import { DiagramBox } from "@/components/marketing/diagram-box"
import { StepFlow } from "@/components/marketing/step-flow"
import { CTASection } from "@/components/marketing/cta-section"
import { Reveal } from "@/components/marketing/reveal"
import { RemoteHelperFlowSvg } from "@/app/diagrams/remote-helper-flow-svg"

export const metadata: Metadata = {
  title: "Remote Services",
  description:
    "Crab's remote helper protocol connects Git to cloud object storage. Push and pull repositories to S3, GCS, or Azure with zero infrastructure.",
  openGraph: {
    title: "Remote Services — Crab",
    description:
      "Crab's remote helper protocol connects Git to cloud object storage. Push and pull repositories to S3, GCS, or Azure with zero infrastructure.",
  },
}

const providerFeatures = [
  {
    icon: Cloud,
    title: "Amazon S3",
    description:
      "Native S3 integration with multipart uploads, server-side encryption, and IAM-based access control. Works with any S3-compatible endpoint.",
  },
  {
    icon: Cloud,
    title: "Google Cloud Storage",
    description:
      "First-class GCS support with resumable uploads, service account authentication, and automatic region selection.",
  },
  {
    icon: Cloud,
    title: "Azure Blob Storage",
    description:
      "Full Azure Blob support with managed identity authentication, block blob uploads, and tiered storage compatibility.",
  },
]

const pushSteps = [
  {
    icon: FileCheck,
    title: "Enumerate Pointers",
    description:
      "Scan committed pointer blobs to identify files that need to be pushed to cloud storage.",
  },
  {
    icon: Layers,
    title: "Chunk & Dedup",
    description:
      "Content-defined chunking splits files into variable-size chunks. 3-tier dedup eliminates redundant data.",
  },
  {
    icon: Package,
    title: "Pack Xorbs",
    description:
      "New chunks are packed into compressed xorb objects (~64 MiB batches) for efficient transfer.",
  },
  {
    icon: Upload,
    title: "Upload & Index",
    description:
      "Xorbs are uploaded to your bucket. Shard and file indexes are updated atomically.",
  },
  {
    icon: GitBranch,
    title: "Update Refs",
    description:
      "Ref CAS (compare-and-swap) ensures safe concurrent pushes. Manifests are updated last.",
  },
]

const pullSteps = [
  {
    icon: Download,
    title: "Fetch Refs",
    description:
      "Download the latest manifest and ref pointers from cloud storage.",
  },
  {
    icon: Server,
    title: "Resolve Pointers",
    description:
      "Map pointer blobs to file-index entries, then to shard locations and xorb chunks.",
  },
  {
    icon: ArrowRightLeft,
    title: "Download Xorbs",
    description:
      "Fetch only the xorb chunks needed for the requested files. Range requests minimize bandwidth.",
  },
  {
    icon: FileCheck,
    title: "Reconstruct",
    description:
      "Reassemble byte-identical files from chunks, verified by Blake3 hash. Lazy checkout defers until hydration.",
  },
]

export default function RemoteServicesPage() {
  return (
    <MarketingLayout>
      {/* Hero */}
      <HeroSection
        badge={{ text: "Cloud Storage Protocol", dot: true }}
        headline={
          <>
            Git remote helper for{" "}
            <span className="text-primary">cloud object storage.</span>
          </>
        }
        subheadline="Crab's remote helper protocol bridges Git and your cloud bucket. Push and pull repositories to S3, GCS, or Azure using standard Git commands — no servers, no endpoints, no infrastructure to manage."
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

      {/* Cloud Provider Feature Cards */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              Supported Cloud Providers
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              First-class support for the three major cloud object storage
              platforms.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <div className="grid grid-cols-1 gap-6 sm:grid-cols-2 lg:grid-cols-3">
            {providerFeatures.map((feature) => (
              <FeatureCard
                key={feature.title}
                icon={feature.icon}
                title={feature.title}
                description={feature.description}
              />
            ))}
          </div>
        </Reveal>
      </section>

      {/* Remote Helper Flow Diagram */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              Remote Helper Protocol
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              How Git communicates with cloud storage through Crab.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <DiagramBox maxWidth={800}>
            <RemoteHelperFlowSvg />
          </DiagramBox>
        </Reveal>
      </section>

      {/* Push Pipeline */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              Push Pipeline
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              From local commits to cloud storage in five steps.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <StepFlow steps={pushSteps} />
        </Reveal>
      </section>

      {/* Pull Pipeline */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <h2 className="text-3xl font-bold tracking-tight text-foreground">
              Pull Pipeline
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              Efficient fetch and reconstruction from cloud storage.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <StepFlow steps={pullSteps} />
        </Reveal>
      </section>

      {/* CTA */}
      <Reveal>
        <CTASection
          headline="Connect your repositories to the cloud"
          description="Start using Crab's remote helper to push and pull Git repositories directly to cloud object storage. No servers required."
          primaryCTA={{
            label: "Read the Documentation",
            href: "/docs/cli",
            icon: BookOpen,
          }}
          secondaryCTA={{
            label: "Contact Us",
            href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog",
            icon: MailIcon,
          }}
        />
      </Reveal>
    </MarketingLayout>
  )
}
