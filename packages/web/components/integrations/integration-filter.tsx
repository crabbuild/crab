"use client"

import { useState, useMemo } from "react"

import { cn } from "@/lib/utils"
import {
  filterIntegrations,
  categoryLabels,
  type Integration,
  type IntegrationCategory,
} from "@/lib/integrations"

interface IntegrationFilterProps {
  integrations: Integration[]
  children: (filtered: Integration[]) => React.ReactNode
}

const categories: IntegrationCategory[] = [
  "cloud",
  "ci-cd",
  "ml",
  "vcs",
]

export function IntegrationFilter({
  integrations,
  children,
}: IntegrationFilterProps) {
  const [active, setActive] = useState("")

  const filtered = useMemo(
    () => filterIntegrations(active, integrations),
    [active, integrations]
  )

  return (
    <div className="space-y-8">
      {/* Category filter buttons */}
      <div
        className="flex flex-wrap items-center gap-2"
        role="group"
        aria-label="Filter integrations by category"
      >
        <button
          onClick={() => setActive("")}
          className={cn(
            "rounded-full px-4 py-1.5 text-sm font-medium transition-colors duration-(--duration-fast)",
            !active
              ? "bg-primary text-primary-foreground"
              : "bg-muted text-muted-foreground hover:bg-muted/80 hover:text-foreground"
          )}
        >
          All
        </button>
        {categories.map((cat) => (
          <button
            key={cat}
            onClick={() => setActive(cat)}
            className={cn(
              "rounded-full px-4 py-1.5 text-sm font-medium transition-colors duration-(--duration-fast)",
              active === cat
                ? "bg-primary text-primary-foreground"
                : "bg-muted text-muted-foreground hover:bg-muted/80 hover:text-foreground"
            )}
          >
            {categoryLabels[cat]}
          </button>
        ))}
      </div>

      {/* Filtered content with fade transition */}
      <div key={active || "all"} className="animate-in fade-in duration-300">
        {filtered.length === 0 ? (
          <p className="py-12 text-center text-muted-foreground">
            No integrations found for this category.
          </p>
        ) : (
          children(filtered)
        )}
      </div>
    </div>
  )
}
