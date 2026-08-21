import type { Metadata } from "next"

import { MarketingLayout } from "@/components/marketing-layout"
import { HeroSection } from "@/components/marketing/hero-section"
import { Reveal } from "@/components/marketing/reveal"
import { ChangelogEntry } from "@/components/changelog/changelog-entry"
import { sortChangelog, changelogData } from "@/lib/changelog"

export const metadata: Metadata = {
  title: "Changelog — Crab",
  description:
    "Track verified product updates, fixes, and release artifacts across Crab releases.",
  openGraph: {
    title: "Changelog — Crab",
    description:
      "Track verified product updates, fixes, and release artifacts across Crab releases.",
  },
}

const ENTRIES_PER_PAGE = 10

export default function ChangelogPage() {
  const entries = sortChangelog(changelogData)

  if (entries.length === 0) {
    return (
      <MarketingLayout>
        <HeroSection
          headline="Changelog"
          subheadline="Track verified product updates, fixes, and release artifacts across Crab releases."
        />
        <section className="mx-auto max-w-3xl px-6 pb-24">
          <p className="py-12 text-center text-muted-foreground">
            No releases available yet. Check back soon.
          </p>
        </section>
      </MarketingLayout>
    )
  }

  // Simple pagination: show first page of entries.
  // When we have >10 entries, only the first page is rendered.
  const paginatedEntries = entries.slice(0, ENTRIES_PER_PAGE)
  const hasMore = entries.length > ENTRIES_PER_PAGE

  return (
    <MarketingLayout>
      <HeroSection
        headline="Changelog"
        subheadline="Only published or repository-backed entries are listed here. Each release links to its source notes."
      />

      <section className="mx-auto max-w-3xl px-6 pb-24">
        <Reveal>
          <div className="space-y-0">
            {paginatedEntries.map((entry, idx) => (
              <ChangelogEntry
                key={entry.version}
                {...entry}
                isLast={idx === paginatedEntries.length - 1}
              />
            ))}
          </div>
        </Reveal>

        {hasMore && (
          <div className="mt-12 text-center">
            <p className="text-sm text-muted-foreground">
              Showing {ENTRIES_PER_PAGE} of {entries.length} releases.
            </p>
          </div>
        )}
      </section>
    </MarketingLayout>
  )
}
