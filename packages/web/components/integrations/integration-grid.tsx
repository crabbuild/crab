"use client"

import { IntegrationFilter } from "@/components/integrations/integration-filter"
import { IntegrationCard } from "@/components/integrations/integration-card"
import type { Integration } from "@/lib/integrations"

interface IntegrationGridProps {
  integrations: Integration[]
}

export function IntegrationGrid({ integrations }: IntegrationGridProps) {
  return (
    <IntegrationFilter integrations={integrations}>
      {(filtered) => (
        <div className="grid gap-6 sm:grid-cols-2 lg:grid-cols-3">
          {filtered.map((integration) => (
            <IntegrationCard key={integration.id} integration={integration} />
          ))}
        </div>
      )}
    </IntegrationFilter>
  )
}
