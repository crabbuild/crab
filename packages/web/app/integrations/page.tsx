import type { Metadata } from "next"
import { MailIcon } from "lucide-react"

import { MarketingLayout } from "@/components/marketing-layout"
import { HeroSection } from "@/components/marketing/hero-section"
import { Reveal } from "@/components/marketing/reveal"
import { CTASection } from "@/components/marketing/cta-section"
import { IntegrationGrid } from "@/components/integrations/integration-grid"
import { integrations } from "@/lib/integrations"

export const metadata: Metadata = {
  title: "Integrations — Crab",
  description:
    "Explore tools and platforms that integrate with Crab — cloud providers, CI/CD, ML frameworks, and more.",
  openGraph: {
    title: "Integrations — Crab",
    description:
      "Explore tools and platforms that integrate with Crab — cloud providers, CI/CD, ML frameworks, and more.",
  },
}

export default function IntegrationsPage() {
  return (
    <MarketingLayout>
      <HeroSection
        headline="Integrations"
        subheadline="Crab works with the tools you already use — cloud storage, CI/CD pipelines, ML frameworks, and development tools."
      />

      <section className="mx-auto max-w-6xl px-6 pb-24">
        <Reveal>
          <IntegrationGrid integrations={integrations} />
        </Reveal>
      </section>

      <CTASection
        headline="Don't see your tool?"
        description="Crab works with any Git-compatible workflow. Check the docs or open an issue to request an integration guide."
        primaryCTA={{ label: "Read the Docs", href: "/docs" }}
        secondaryCTA={{ label: "Contact Us", href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog", icon: MailIcon }}
      />
    </MarketingLayout>
  )
}
