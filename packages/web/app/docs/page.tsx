import type { Metadata } from "next"
import {
  ArrowRight,
  BookOpen,
  Bug,
  CloudCog,
  FolderSync,
  Rocket,
  Terminal,
  Workflow,
} from "lucide-react"
import Link from "next/link"

import { MarketingLayout } from "@/components/marketing-layout"
import { HeroSection } from "@/components/marketing/hero-section"
import { Reveal } from "@/components/marketing/reveal"
import { DocsCategoryCard } from "@/components/docs/docs-category-card"

export const metadata: Metadata = {
  title: "Documentation",
  description:
    "Explore Crab documentation — CLI guides, workflows, architecture, and more.",
}

/* ─── CLI doc categories ─── */

const cliCategories = [
  {
    icon: Rocket,
    name: "Getting Started",
    description: "Install Crab, create your first repository, and push to cloud storage.",
    href: "/docs/cli/getting-started",
  },
  {
    icon: FolderSync,
    name: "Daily Workflow",
    description: "Add, push, hydrate, dehydrate — the commands you use every day.",
    href: "/docs/cli/daily-workflow",
  },
  {
    icon: BookOpen,
    name: "Guides",
    description: "Migrate from LFS/DVC, CI/CD integration, and sharing repositories.",
    href: "/docs/cli/guides",
  },
  {
    icon: CloudCog,
    name: "Managed Service",
    description: "Hosted repositories, access management, migration, and service APIs.",
    href: "/docs/cli/managed-service",
  },
  {
    icon: Workflow,
    name: "Automation",
    description: "Scripting, CI pipelines, and workflow automation with Crab.",
    href: "/docs/cli/automation",
  },
  {
    icon: Bug,
    name: "Diagnostics",
    description: "Health checks, troubleshooting, and debugging Crab operations.",
    href: "/docs/cli/diagnostics",
  },
]

export default function DocsLandingPage() {
  return (
    <MarketingLayout>
      <HeroSection
        headline="Documentation"
        subheadline="Everything you need to get started with Crab — the serverless git remote for large files in cloud object storage."
      />

      <section className="mx-auto max-w-6xl px-6 pb-24">
        {/* CLI Section */}
        <Reveal>
          <div className="mb-16">
            <div className="mb-6 flex items-center gap-3">
              <Terminal className="size-5 text-primary" />
              <h2 className="text-2xl font-semibold text-foreground">Crab CLI</h2>
              <Link
                href="/docs/cli/getting-started"
                className="ml-auto flex items-center gap-1 text-sm text-muted-foreground transition-colors hover:text-primary"
              >
                View all CLI docs
                <ArrowRight className="size-3.5" />
              </Link>
            </div>
            <div className="grid gap-6 sm:grid-cols-2 lg:grid-cols-3">
              {cliCategories.map((category) => (
                <DocsCategoryCard
                  key={category.name}
                  icon={category.icon}
                  name={category.name}
                  description={category.description}
                  href={category.href}
                />
              ))}
            </div>
          </div>
        </Reveal>

      </section>
    </MarketingLayout>
  )
}
