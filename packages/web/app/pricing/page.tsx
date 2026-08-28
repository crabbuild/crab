import { Fragment } from "react"
import {
  ArrowRight,
  Bell,
  Check,
  Download,
  GitBranch,
  Minus,
} from "lucide-react"

import { MarketingLayout } from "@/components/marketing-layout"
import { CTASection } from "@/components/marketing/cta-section"
import { Reveal } from "@/components/marketing/reveal"
import { ResponsiveTableWrapper } from "@/components/marketing/responsive-table-wrapper"
import { CostCalculator } from "@/components/pricing/cost-calculator"
import { CostBreakdownSvg } from "@/app/diagrams/cost-breakdown-svg"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Table,
  TableHeader,
  TableBody,
  TableRow,
  TableHead,
  TableCell,
} from "@/components/ui/table"
import { pricingData } from "@/lib/pricing-data"
import { createPageMetadata } from "@/lib/metadata"

export const metadata = createPageMetadata({
  title: "Pricing",
  description:
    "Crab is free for developers. Enterprise auth, managed caching, audit logs, and priority support are coming soon.",
  path: "/pricing",
})

/* ─── Tier card data ─── */

interface TierFeature {
  text: string
  included: boolean
}

interface Tier {
  name: string
  status: string
  price: string
  priceSuffix?: string
  description: string
  cta: { label: string; href: string }
  features: TierFeature[]
  available: boolean
}

const tiers: Tier[] = [
  {
    name: "Developer",
    status: "Available now",
    price: "$0",
    priceSuffix: "forever",
    description:
      "The complete open-source CLI. You only pay your cloud provider for storage and requests.",
    cta: {
      label: "Get Started",
      href: "/docs/cli/getting-started/installation",
    },
    available: true,
    features: [
      { text: "Unlimited repos & file size", included: true },
      { text: "All cloud providers (S3, GCS, Azure)", included: true },
      { text: "Content-defined chunking & 3-tier dedup", included: true },
      { text: "Lazy checkout & FUSE mount", included: true },
      { text: "Git LFS compatibility", included: true },
      { text: "Community support", included: true },
      { text: "SSO / SAML / OIDC", included: false },
      { text: "Managed cache service", included: false },
      { text: "Audit logs", included: false },
    ],
  },
  {
    name: "Enterprise",
    status: "Coming soon",
    price: "Coming Soon",
    description:
      "Managed auth, caching, coordination, and production support for teams. Join the waitlist for launch updates.",
    cta: {
      label: "Join the Waitlist",
      href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog",
    },
    available: false,
    features: [
      { text: "Everything in Developer", included: true },
      {
        text: "SSO via OIDC, SAML, Azure Entra, GCP Federation",
        included: true,
      },
      {
        text: "Credential vending (short-lived scoped tokens)",
        included: true,
      },
      { text: "Repo-level RBAC & access policies", included: true },
      { text: "Managed chunk cache (10–50× faster hydrate)", included: true },
      { text: "Regional cache nodes", included: true },
      { text: "Audit log with 90-day retention", included: true },
      { text: "Distributed push locking & pipelined commits", included: true },
      { text: "Priority support (4h SLA)", included: true },
      { text: "Dedicated onboarding & migration assistance", included: true },
    ],
  },
]

/* ─── Feature comparison data ─── */

interface FeatureRow {
  feature: string
  category: string
  developer: string | boolean
  enterprise: string | boolean
}

const featureComparison: FeatureRow[] = [
  // Core
  { feature: "Cloud storage push/pull", category: "Core", developer: true, enterprise: true },
  { feature: "Content-defined chunking & dedup", category: "Core", developer: true, enterprise: true },
  { feature: "Lazy checkout & FUSE mount", category: "Core", developer: true, enterprise: true },
  { feature: "All cloud providers (S3, GCS, Azure)", category: "Core", developer: true, enterprise: true },
  { feature: "Git LFS compatibility", category: "Core", developer: true, enterprise: true },
  // Auth & Access
  { feature: "Cloud IAM (bucket-level)", category: "Auth & Access", developer: true, enterprise: true },
  { feature: "SSO (OIDC / SAML / Azure Entra)", category: "Auth & Access", developer: false, enterprise: true },
  { feature: "Credential vending service", category: "Auth & Access", developer: false, enterprise: true },
  { feature: "Repo-level RBAC", category: "Auth & Access", developer: false, enterprise: true },
  { feature: "Audit log", category: "Auth & Access", developer: false, enterprise: true },
  // Performance
  { feature: "Local chunk cache", category: "Performance", developer: true, enterprise: true },
  { feature: "Managed cache service", category: "Performance", developer: false, enterprise: true },
  { feature: "Regional cache nodes", category: "Performance", developer: false, enterprise: true },
  { feature: "Cache warming & pre-fetch", category: "Performance", developer: false, enterprise: true },
  // Collaboration
  { feature: "Distributed push locking", category: "Collaboration", developer: false, enterprise: true },
  { feature: "Pipelined commits", category: "Collaboration", developer: false, enterprise: true },
  { feature: "Team activity feed", category: "Collaboration", developer: false, enterprise: true },
  { feature: "Remote SSH workspaces", category: "Collaboration", developer: false, enterprise: true },
  // Support
  { feature: "Community support (GitHub)", category: "Support", developer: true, enterprise: true },
  { feature: "Priority support (4h SLA)", category: "Support", developer: false, enterprise: true },
  { feature: "Dedicated onboarding", category: "Support", developer: false, enterprise: true },
]

/* ─── FAQ data ─── */

interface FAQItem {
  question: string
  answer: string
}

const faqItems: FAQItem[] = [
  {
    question: "Is the CLI really free? What's the catch?",
    answer:
      "No catch. The Crab CLI is open-source and free forever. You pay only your cloud provider's standard storage and request costs — the same rates you'd pay using S3, GCS, or Azure directly. Enterprise managed services are coming soon.",
  },
  {
    question: "When will Enterprise be available?",
    answer:
      "Enterprise is coming soon. Join the waitlist to hear when early access opens and to receive launch and pricing updates.",
  },
  {
    question: "What is planned for Enterprise?",
    answer:
      "The planned Enterprise control plane adds SSO and credential vending, a distributed chunk cache, coordination services for concurrent pushes, audit logs, and priority support. Your repository data will remain in your own cloud bucket.",
  },
  {
    question: "How will Enterprise be priced?",
    answer:
      "Final Enterprise packaging and pricing have not been announced. Join the waitlist and we'll share the details before launch.",
  },
  {
    question: "What are data transfer (egress) costs?",
    answer:
      "Cloud providers charge for data leaving their network. When you pull (hydrate) files, you pay egress fees — typically $0.09/GB for AWS S3. Crab's chunk-level deduplication minimizes egress by only downloading changed chunks. The planned Enterprise cache will also reduce repeated downloads from origin storage.",
  },
  {
    question: "Do I still pay cloud storage costs on Enterprise?",
    answer:
      "Yes. Cloud storage costs remain separate because your data stays in your bucket and you pay your provider directly. Enterprise pricing will cover the managed services around that storage. The calculator below estimates the cloud portion of your costs.",
  },
  {
    question: "Can my team use Crab today?",
    answer:
      "Yes. Teams can use the free CLI today with their existing cloud IAM and bucket policies. Enterprise will add managed identity, caching, coordination, and support when it launches.",
  },
]

/* ─── Cloud storage reference table component ─── */

function StorageReferenceTable({
  providerId,
  title,
  recommendedTier,
}: {
  providerId: string
  title: string
  recommendedTier: string
}) {
  const provider = pricingData.providers.find((p) => p.id === providerId)
  if (!provider) return null
  const region = provider.regions[0]
  if (!region) return null

  return (
    <div className="mb-8">
      <h3 className="mb-3 text-base font-semibold text-foreground">
        {title} — {region.name}
      </h3>
      <ResponsiveTableWrapper>
        <Table>
          <TableHeader>
            <TableRow className="border-border">
              <TableHead>Class</TableHead>
              <TableHead>Storage/GB/mo</TableHead>
              <TableHead>PUT/1K ops</TableHead>
              <TableHead>GET/1K ops</TableHead>
              <TableHead>Egress/GB</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {region.storageClasses.map((cls, idx) => {
              const isRecommended = cls.name === recommendedTier
              return (
                <TableRow
                  key={cls.id}
                  className={
                    isRecommended
                      ? "bg-primary/10 font-medium"
                      : idx % 2 === 1
                        ? "bg-muted/50"
                        : ""
                  }
                >
                  <TableCell className="font-medium">
                    <span className="flex items-center gap-2">
                      {cls.name}
                      {isRecommended && (
                        <Badge variant="default" className="text-xs">
                          recommended
                        </Badge>
                      )}
                    </span>
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    ${cls.storageCostPerGbMonth.toFixed(4)}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    ${cls.putCostPer1kOps.toFixed(3)}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    ${cls.getCostPer1kOps.toFixed(4)}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    ${cls.egressCostPerGb.toFixed(3)}
                  </TableCell>
                </TableRow>
              )
            })}
          </TableBody>
        </Table>
      </ResponsiveTableWrapper>
    </div>
  )
}

/* ─── Feature cell renderer ─── */

function FeatureCell({
  value,
  includedLabel = "Included",
}: {
  value: string | boolean
  includedLabel?: string
}) {
  if (value === true) {
    return <Check className="size-5 text-primary" aria-label={includedLabel} />
  }
  if (value === false) {
    return (
      <Minus
        className="size-5 text-muted-foreground"
        aria-label="Not included"
      />
    )
  }
  return <span>{value}</span>
}

/* ─── Page ─── */

export default function PricingPage() {
  return (
    <MarketingLayout>
      {/* Hero */}
      <section className="border-b border-border/70 bg-linear-to-b from-primary/10 via-background to-background">
        <div className="mx-auto max-w-6xl px-6 pt-32 pb-20 text-center md:pt-40 md:pb-24">
          <Reveal>
            <Badge variant="outline" className="mb-5 bg-background/80">
              Simple, transparent pricing
            </Badge>
            <h1 className="text-heading-hero font-bold tracking-tight text-foreground">
              One free CLI.
              <br />
              Enterprise is on the way.
            </h1>
            <p className="mx-auto mt-5 max-w-2xl text-lg leading-relaxed text-muted-foreground">
              Crab is free forever. Bring your own cloud bucket and pay your
              provider directly. Managed services for teams are coming soon.
            </p>

            <div className="mx-auto mt-10 max-w-2xl rounded-(--card-radius) border border-border bg-card/90 p-5 text-left shadow-card backdrop-blur sm:p-6">
              <div className="grid grid-cols-[1fr_48px_1fr] items-center gap-3 sm:grid-cols-[1fr_88px_1fr] sm:gap-5">
                <div className="flex items-center gap-3">
                  <span className="flex size-9 shrink-0 items-center justify-center rounded-full bg-primary text-primary-foreground shadow-sm">
                    <Check className="size-4" aria-hidden="true" />
                  </span>
                  <div>
                    <p className="font-mono text-[0.625rem] tracking-[0.18em] text-primary uppercase">
                      Available now
                    </p>
                    <p className="mt-1 text-sm font-semibold text-foreground">
                      Developer
                    </p>
                  </div>
                </div>
                <div className="flex items-center" aria-hidden="true">
                  <span className="h-px flex-1 border-t border-dashed border-primary/50" />
                  <GitBranch className="mx-2 size-4 text-primary" />
                  <span className="h-px flex-1 border-t border-dashed border-primary/50" />
                </div>
                <div className="flex items-center justify-end gap-3">
                  <div className="text-right">
                    <p className="font-mono text-[0.625rem] tracking-[0.18em] text-muted-foreground uppercase">
                      Coming soon
                    </p>
                    <p className="mt-1 text-sm font-semibold text-foreground">
                      Enterprise
                    </p>
                  </div>
                  <span className="size-9 shrink-0 rounded-full border-2 border-dashed border-primary/50 bg-primary/5" />
                </div>
              </div>
            </div>
          </Reveal>
        </div>
      </section>

      {/* Pricing Tier Cards */}
      <section className="mx-auto max-w-6xl px-6 py-(--section-padding)">
        <Reveal>
          <div className="mb-10 max-w-2xl">
            <p className="font-mono text-xs font-semibold tracking-[0.16em] text-primary uppercase">
              Plans
            </p>
            <h2 className="mt-3 text-heading-lg font-bold tracking-tight text-foreground">
              Start today. Scale when you need to.
            </h2>
            <p className="mt-3 text-muted-foreground">
              The Developer plan is the full Crab CLI, not a limited trial.
              Enterprise adds the managed layer teams need at scale.
            </p>
          </div>

          <div className="grid gap-6 lg:grid-cols-2">
            {tiers.map((tier) => (
              <div
                key={tier.name}
                className={`relative flex flex-col overflow-hidden rounded-(--card-radius) border p-6 transition-all duration-(--duration-fast) sm:p-8 ${
                  tier.available
                    ? "border-primary/50 bg-card shadow-md"
                    : "border-dashed border-border bg-muted/30 shadow-sm"
                }`}
              >
                {tier.available && (
                  <div className="absolute inset-x-0 top-0 h-1 bg-primary" />
                )}
                <div className="flex items-start justify-between gap-4">
                  <div>
                    <p className="font-mono text-[0.625rem] font-semibold tracking-[0.18em] text-muted-foreground uppercase">
                      {tier.name === "Developer" ? "Open source" : "For teams"}
                    </p>
                    <h3 className="mt-2 text-2xl font-bold text-foreground">
                      {tier.name}
                    </h3>
                  </div>
                  <Badge
                    variant={tier.available ? "default" : "outline"}
                    className={
                      tier.available ? "" : "border-dashed bg-background"
                    }
                  >
                    {tier.status}
                  </Badge>
                </div>
                <div className="mt-6 flex min-h-12 items-baseline gap-2">
                  <span
                    className={
                      tier.available
                        ? "text-5xl font-bold tracking-tight text-foreground"
                        : "text-3xl font-bold tracking-tight text-foreground"
                    }
                  >
                    {tier.price}
                  </span>
                  {tier.priceSuffix && (
                    <span className="text-sm font-medium text-muted-foreground">
                      {tier.priceSuffix}
                    </span>
                  )}
                </div>
                <p className="mt-4 max-w-lg text-sm leading-relaxed text-muted-foreground">
                  {tier.description}
                </p>

                <Button
                  variant={tier.available ? "default" : "outline"}
                  size="lg"
                  className="mt-7 w-full"
                  render={<a href={tier.cta.href} />}
                >
                  {tier.cta.label}
                  {tier.available ? (
                    <ArrowRight className="ml-1 size-4" />
                  ) : (
                    <Bell className="ml-1 size-4" />
                  )}
                </Button>

                <p className="mt-8 border-t border-border pt-6 font-mono text-[0.625rem] font-semibold tracking-[0.16em] text-muted-foreground uppercase">
                  {tier.available ? "Included today" : "Planned for launch"}
                </p>
                <ul className="mt-4 flex-1 space-y-3">
                  {tier.features.map((f) => (
                    <li key={f.text} className="flex items-start gap-3 text-sm">
                      {f.included ? (
                        <Check className="mt-0.5 size-4 shrink-0 text-primary" />
                      ) : (
                        <Minus className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
                      )}
                      <span
                        className={
                          f.included
                            ? "text-foreground"
                            : "text-muted-foreground"
                        }
                      >
                        {f.text}
                      </span>
                    </li>
                  ))}
                </ul>

                {!tier.available && (
                  <p className="mt-5 rounded-md border border-dashed border-border bg-background px-3 py-2 text-xs leading-relaxed text-muted-foreground">
                    Launch timing and final pricing will be shared with waitlist
                    members first.
                  </p>
                )}
              </div>
            ))}
          </div>
        </Reveal>
      </section>

      {/* Detailed Feature Comparison Table */}
      <section className="border-y border-border/70 bg-muted/30">
        <Reveal className="mx-auto max-w-6xl px-6 py-(--section-padding)">
          <h2 className="mb-2 text-heading-lg font-bold tracking-tight text-foreground">
            Compare what ships today with what&apos;s next
          </h2>
          <p className="mb-8 text-muted-foreground">
            Enterprise capabilities below are planned and will become available
            when the plan launches.
          </p>
          <ResponsiveTableWrapper>
            <Table>
              <TableHeader>
                <TableRow className="border-border">
                  <TableHead className="min-w-[240px]">Feature</TableHead>
                  <TableHead className="text-center">
                    <div className="flex flex-col items-center gap-1">
                      <span className="font-semibold">Developer</span>
                      <span className="text-xs font-normal text-muted-foreground">
                        Free
                      </span>
                    </div>
                  </TableHead>
                  <TableHead className="text-center">
                    <div className="flex flex-col items-center gap-1">
                      <span className="font-semibold">Enterprise</span>
                      <Badge
                        variant="outline"
                        className="border-dashed bg-background"
                      >
                        Coming soon
                      </Badge>
                    </div>
                  </TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {featureComparison.map((row, idx) => {
                  const showCategory =
                    idx === 0 ||
                    row.category !== featureComparison[idx - 1]?.category
                  return (
                    <Fragment key={row.feature}>
                      {showCategory && (
                        <TableRow className="bg-muted/70">
                          <TableCell
                            colSpan={3}
                            className="text-xs font-semibold tracking-wide text-muted-foreground uppercase"
                          >
                            {row.category}
                          </TableCell>
                        </TableRow>
                      )}
                      <TableRow className={idx % 2 === 0 ? "" : "bg-muted/30"}>
                        <TableCell className="font-medium">
                          {row.feature}
                        </TableCell>
                        <TableCell className="text-center">
                          <span className="flex items-center justify-center">
                            <FeatureCell value={row.developer} />
                          </span>
                        </TableCell>
                        <TableCell className="text-center">
                          <span className="flex items-center justify-center">
                            <FeatureCell
                              value={row.enterprise}
                              includedLabel="Planned for Enterprise"
                            />
                          </span>
                        </TableCell>
                      </TableRow>
                    </Fragment>
                  )
                })}
              </TableBody>
            </Table>
          </ResponsiveTableWrapper>
        </Reveal>
      </section>

      {/* Cost Calculator */}
      <section className="mx-auto max-w-6xl px-6 py-(--section-padding)">
        <Reveal>
          <h2 className="mb-2 text-heading-lg font-bold tracking-tight text-foreground">
            Cloud Storage Cost Calculator
          </h2>
          <p className="mb-8 text-muted-foreground">
            Cloud storage is billed by your provider, separately from Crab.
            Estimate that monthly spend below.
          </p>
          <CostCalculator />
        </Reveal>
      </section>

      {/* Cost Breakdown Diagram */}
      <section className="mx-auto max-w-3xl px-6 py-12">
        <Reveal>
          <CostBreakdownSvg />
        </Reveal>
      </section>

      {/* FAQ */}
      <section className="mx-auto max-w-6xl px-6 py-(--section-padding)">
        <Reveal>
          <h2 className="mb-2 text-heading-lg font-bold tracking-tight text-foreground">
            Frequently Asked Questions
          </h2>
          <p className="mb-8 text-muted-foreground">
            What is free today, what your cloud provider charges, and what to
            expect from Enterprise.
          </p>
          <div className="space-y-3">
            {faqItems.map((item) => (
              <details
                key={item.question}
                className="group rounded-(--card-radius) border border-border bg-card transition-shadow duration-(--duration-fast) open:shadow-sm"
              >
                <summary className="flex cursor-pointer items-center justify-between p-(--card-padding) text-sm font-medium text-foreground select-none [&::-webkit-details-marker]:hidden">
                  <span>{item.question}</span>
                  <span
                    className="ml-4 shrink-0 text-muted-foreground transition-transform duration-(--duration-normal) group-open:rotate-45"
                    aria-hidden="true"
                  >
                    +
                  </span>
                </summary>
                <div className="px-(--card-padding) pb-(--card-padding) text-sm leading-relaxed text-muted-foreground">
                  {item.answer}
                </div>
              </details>
            ))}
          </div>
        </Reveal>
      </section>

      {/* Cloud Storage Reference Tables */}
      <section className="mx-auto max-w-6xl px-6 py-(--section-padding)">
        <Reveal>
          <h2 className="mb-2 text-heading-lg font-bold tracking-tight text-foreground">
            Cloud Storage Reference Pricing
          </h2>
          <p className="mb-8 text-muted-foreground">
            Current rates from major cloud providers. The recommended class for
            most Crab workloads is highlighted.
          </p>

          <StorageReferenceTable
            providerId="aws-s3"
            title="AWS S3"
            recommendedTier="Standard"
          />
          <StorageReferenceTable
            providerId="gcs"
            title="Google Cloud Storage"
            recommendedTier="Standard"
          />
          <StorageReferenceTable
            providerId="azure-blob"
            title="Azure Blob Storage"
            recommendedTier="Hot"
          />

          <p className="mt-4 text-xs text-muted-foreground">
            Pricing data sourced from cloud provider published rates and may
            change. Price table version: <strong>2026-03-01</strong>.
          </p>
        </Reveal>
      </section>

      <CTASection
        headline="Start free. Stay in the loop."
        description="Install the complete CLI today, or join the Enterprise waitlist for launch updates."
        primaryCTA={{
          label: "Download CLI",
          href: "/docs/cli/getting-started/installation",
          icon: Download,
        }}
        secondaryCTA={{
          label: "Join the Waitlist",
          href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog",
          icon: Bell,
        }}
        variant="accent"
      />
    </MarketingLayout>
  )
}
