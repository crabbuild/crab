import type { Metadata } from "next"
import Link from "next/link"
import { ArrowRight, Cloud, GitBranch, ShieldCheck } from "lucide-react"

import { LegalPage, LegalSection } from "@/components/marketing/legal-page"
import { Button } from "@/components/ui/button"

export const metadata: Metadata = {
  title: "About Us",
  description:
    "Learn about Beyondnote Technology Inc, the team building Crab: a serverless Git remote helper for large repositories in cloud object storage.",
  openGraph: {
    title: "About Us - Crab",
    description:
      "Crab is built for teams that need Git workflows for large files without a hosted LFS control plane.",
  },
}

const sectionLinks = [
  { id: "who-we-are", label: "Who we are" },
  { id: "why-crab-exists", label: "Why Crab exists" },
  { id: "how-we-build", label: "How we build" },
  { id: "trust-boundary", label: "Trust boundary" },
]

const summaryItems = [
  {
    icon: GitBranch,
    title: "Git first",
    description:
      "Crab works through standard Git concepts: remote helpers, filter drivers, pointers, fetch, push, clone, and hydration.",
  },
  {
    icon: Cloud,
    title: "Cloud native",
    description:
      "Repository data is stored in customer controlled object storage such as S3, Google Cloud Storage, Azure Blob Storage, or compatible endpoints.",
  },
  {
    icon: ShieldCheck,
    title: "Boundary aware",
    description:
      "The CLI is designed so normal repository data movement stays between the developer machine and the configured cloud bucket.",
  },
]

export default function AboutUsPage() {
  return (
    <LegalPage
      eyebrow="About Crab"
      title="Serverless Git for repositories that outgrew ordinary Git."
      intro="Crab is built by Beyondnote Technology Inc for developers working with datasets, model checkpoints, media, build artifacts, and other large files that still belong in a Git-shaped workflow."
      summaryItems={summaryItems}
      sectionLinks={sectionLinks}
    >
      <LegalSection id="who-we-are" title="Who we are">
        <p>
          Beyondnote Technology Inc builds Crab, a serverless Git remote helper
          and large-file workflow for teams that want to use their own cloud
          storage instead of running a separate Git LFS service.
        </p>
        <p>
          The project is centered on a Rust CLI that can act as{" "}
          <code>git-remote-crab</code>, a Git filter driver, and a set of
          repository maintenance commands. Crab also includes docs and optional
          enterprise services for authentication, caching, and operational
          support.
        </p>
      </LegalSection>

      <LegalSection id="why-crab-exists" title="Why Crab exists">
        <p>
          Large files make ordinary Git slow and expensive. Git LFS helps, but
          it usually introduces a hosted endpoint or service contract between
          developers and their data.
        </p>
        <p>
          Crab takes a different shape: Git stores small pointer blobs, while
          actual file content is chunked, deduplicated, compressed, and stored
          in object storage that the user or organization controls. Developers
          can clone quickly, hydrate only the files they need, and keep standard
          Git commands in the loop.
        </p>
      </LegalSection>

      <LegalSection id="how-we-build" title="How we build">
        <p>
          We optimize for boring operational boundaries: resumable uploads,
          content-addressed chunks, byte-identical reconstruction, explicit
          cloud credentials, local caches, and repository-scoped metadata.
        </p>
        <p>
          The CLI supports AWS S3, Google Cloud Storage, Azure Blob Storage, and
          S3-compatible systems. For larger teams, optional Crab Auth and cache
          services can be deployed by the customer to add identity-based
          authorization, audit trails, and shared cache acceleration.
        </p>
      </LegalSection>

      <LegalSection id="trust-boundary" title="Trust boundary">
        <p>
          Crab is designed so the normal data path does not require Crab to host
          repository contents. Your configured cloud bucket is the origin for
          repository data, and your identity provider or cloud IAM policy
          decides who can access it.
        </p>
        <p>
          When users contact us, use the website, download releases, or opt in
          to enterprise support, those interactions have their own privacy and
          service terms. The product boundary is intentional: repository data
          should stay where the customer expects it to stay.
        </p>
      </LegalSection>

      <div className="rounded-lg border bg-primary/5 p-6">
        <p className="text-sm font-semibold text-primary">
          Start with Crab CLI
        </p>
        <h2 className="mt-3 text-xl font-semibold tracking-tight text-foreground">
          See how the serverless remote helper fits into a real Git workflow.
        </h2>
        <p className="mt-3 text-sm leading-6 text-muted-foreground">
          Install the CLI, initialize a bucket-backed remote, and hydrate large
          files only when you need them.
        </p>
        <div className="mt-5 flex flex-wrap gap-3">
          <Button size="lg" render={<Link href="/cli" />}>
            Explore CLI
            <ArrowRight />
          </Button>
          <Button
            variant="outline"
            size="lg"
            render={<Link href="/docs/cli" />}
          >
            Read docs
            <ArrowRight />
          </Button>
        </div>
      </div>
    </LegalPage>
  )
}
