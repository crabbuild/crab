import { Fragment } from "react"
import type { Metadata } from "next"
import { Download, Check, Minus, ArrowRight, MailIcon } from "lucide-react"

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

export const metadata: Metadata = {
  title: "Pricing — Crab",
  description:
    "Crab is free for developers. Enterprise teams get SSO, managed caching, audit logs, and priority support starting at $39/seat/month.",
  openGraph: {
    title: "Pricing — Crab",
    description:
      "Crab is free for developers. Enterprise teams get SSO, managed caching, audit logs, and priority support starting at $39/seat/month.",
  },
}

/* ─── Tier card data ─── */

interface TierFeature {
  text: string
  included: boolean
}

interface Tier {
  name: string
  badge?: string
  price: string
  priceSuffix?: string
  description: string
  cta: { label: string; href: string }
  features: TierFeature[]
  highlighted?: boolean
}

const tiers: Tier[] = [
  {
    name: "Developer",
    badge: "Free Forever",
    price: "$0",
    priceSuffix: "/ month",
    description:
      "Full-featured CLI. You pay only your cloud provider's storage costs.",
    cta: { label: "Get Started", href: "/docs/cli/getting-started/installation" },
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
    badge: "For Teams",
    price: "$39",
    priceSuffix: "/ seat / month",
    description:
      "Everything in Developer plus auth, caching, coordination, and dedicated support for production teams.",
    cta: { label: "Contact Us", href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog" },
    highlighted: true,
    features: [
      { text: "Everything in Developer", included: true },
      { text: "SSO via OIDC, SAML, Azure Entra, GCP Federation", included: true },
      { text: "Credential vending (short-lived scoped tokens)", included: true },
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
      "No catch. The Crab CLI is open-source and free forever. You pay only your cloud provider's standard storage and request costs — the same rates you'd pay using S3/GCS/Azure directly. We monetize through the Enterprise tier which adds managed services (auth, caching, coordination) and support SLAs that teams need at scale.",
  },
  {
    question: "What does the Enterprise tier actually host?",
    answer:
      "Enterprise adds a managed control plane: an auth service for SSO/credential vending, a distributed chunk cache for faster hydrates, coordination services for safe concurrent pushes, and an audit log. Your data still lives in your own cloud bucket — we never store your files. The control plane handles identity, caching, and coordination only.",
  },
  {
    question: "Can I try Enterprise features before committing?",
    answer:
      "Yes. We offer a 14-day free trial for teams of up to 20 seats. No credit card required to start. Fill out our contact form to get set up.",
  },
  {
    question: "How does per-seat pricing work?",
    answer:
      "A seat is any user who authenticates through the Enterprise auth service. CI/CD service accounts count as one seat regardless of how many pipelines use them. Annual billing is $33/seat/month (15% discount). Volume discounts available for 50+ seats.",
  },
  {
    question: "What are data transfer (egress) costs?",
    answer:
      "Cloud providers charge for data leaving their network. When you pull (hydrate) files, you pay egress fees — typically $0.09/GB for AWS S3. Crab's chunk-level deduplication minimizes egress by only downloading changed chunks. Enterprise customers with the managed cache service see significantly reduced egress since repeated pulls are served from cache.",
  },
  {
    question: "Do I still pay cloud storage costs on Enterprise?",
    answer:
      "Yes. The Enterprise fee covers the managed services (auth, cache, coordination, support). Cloud storage costs remain pass-through — your data lives in your bucket and you pay your provider directly. The cost calculator below estimates those cloud costs.",
  },
  {
    question: "What if I need something between Developer and Enterprise?",
    answer:
      "We're considering a Team tier with SSO + cache at a lower price point. If that interests you, reach out via our contact form and we'll keep you in the loop.",
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

function FeatureCell({ value }: { value: string | boolean }) {
  if (value === true) {
    return <Check className="size-5 text-primary" aria-label="Included" />
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
      <section className="mx-auto max-w-5xl px-6 pt-32 pb-12 text-center">
        <Reveal>
          <Badge variant="outline" className="mb-4">
            Simple, transparent pricing
          </Badge>
          <h1 className="text-heading-hero font-bold tracking-tight text-foreground">
            Free for Developers.
            <br />
            Built for Enterprise.
          </h1>
          <p className="mx-auto mt-4 max-w-2xl text-lg text-muted-foreground">
            The CLI is free — no SaaS fee, no per-seat charge.
            Enterprise teams get managed auth, caching, and coordination
            services with priority support.
          </p>
        </Reveal>
      </section>

      {/* Pricing Tier Cards */}
      <section className="mx-auto max-w-5xl px-6 py-12">
        <Reveal>
          <div className="grid gap-8 md:grid-cols-2">
            {tiers.map((tier) => (
              <div
                key={tier.name}
                className={`relative flex flex-col rounded-(--card-radius) border p-(--card-padding) transition-shadow duration-(--duration-fast) ${
                  tier.highlighted
                    ? "border-primary bg-primary/5 shadow-md"
                    : "border-border bg-card shadow-sm"
                }`}
              >
                {tier.badge && (
                  <Badge
                    variant={tier.highlighted ? "default" : "secondary"}
                    className="mb-4 w-fit"
                  >
                    {tier.badge}
                  </Badge>
                )}
                <h2 className="text-2xl font-bold text-foreground">
                  {tier.name}
                </h2>
                <div className="mt-3 flex items-baseline gap-1">
                  <span className="text-4xl font-bold tracking-tight text-foreground">
                    {tier.price}
                  </span>
                  {tier.priceSuffix && (
                    <span className="text-sm text-muted-foreground">
                      {tier.priceSuffix}
                    </span>
                  )}
                </div>
                <p className="mt-3 text-sm text-muted-foreground">
                  {tier.description}
                </p>

                <Button
                  variant={tier.highlighted ? "default" : "outline"}
                  className="mt-6 w-full"
                  render={<a href={tier.cta.href} />}
                >
                  {tier.cta.label}
                  <ArrowRight className="ml-2 size-4" />
                </Button>

                <ul className="mt-6 flex-1 space-y-3 border-t border-border pt-6">
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

                {tier.name === "Enterprise" && (
                  <p className="mt-4 text-xs text-muted-foreground">
                    Annual billing: $33/seat/month. Volume discounts for 50+
                    seats.
                  </p>
                )}
              </div>
            ))}
          </div>
        </Reveal>
      </section>

      {/* Detailed Feature Comparison Table */}
      <section className="mx-auto max-w-5xl px-6 py-(--section-padding)">
        <Reveal>
          <h2 className="mb-2 text-heading-lg font-bold tracking-tight text-foreground">
            Full Feature Comparison
          </h2>
          <p className="mb-8 text-muted-foreground">
            Everything included in each tier, broken down by category.
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
                      <span className="text-xs font-normal text-muted-foreground">
                        $39/seat/mo
                      </span>
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
                              className="text-xs font-semibold uppercase tracking-wide text-muted-foreground"
                            >
                              {row.category}
                            </TableCell>
                          </TableRow>
                        )}
                        <TableRow
                          className={idx % 2 === 0 ? "" : "bg-muted/30"}
                        >
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
                              <FeatureCell value={row.enterprise} />
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
      <section className="mx-auto max-w-5xl px-6 py-(--section-padding)">
        <Reveal>
          <h2 className="mb-2 text-heading-lg font-bold tracking-tight text-foreground">
            Cloud Storage Cost Calculator
          </h2>
          <p className="mb-8 text-muted-foreground">
            Both tiers pay cloud storage costs directly to your provider.
            Estimate your monthly spend below.
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
      <section className="mx-auto max-w-5xl px-6 py-(--section-padding)">
        <Reveal>
          <h2 className="mb-2 text-heading-lg font-bold tracking-tight text-foreground">
            Frequently Asked Questions
          </h2>
          <p className="mb-8 text-muted-foreground">
            Common questions about Crab pricing and the Enterprise tier.
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
      <section className="mx-auto max-w-5xl px-6 py-(--section-padding)">
        <Reveal>
          <h2 className="mb-2 text-heading-lg font-bold tracking-tight text-foreground">
            Cloud Storage Reference Pricing
          </h2>
          <p className="mb-8 text-muted-foreground">
            Current rates from major cloud providers. These apply to both tiers
            — the recommended class for most Crab workloads is highlighted.
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
        headline="Ready to get started?"
        description="Start free with the CLI today, or talk to us about Enterprise for your team."
        primaryCTA={{
          label: "Download CLI",
          href: "/docs/cli/getting-started/installation",
          icon: Download,
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
