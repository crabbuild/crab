import Link from "next/link"
import {
  ArrowUpRight,
  Bot,
  Cloud,
  Code,
  Container,
  Database,
  FlaskConical,
  GitBranch,
  Laptop,
  LineChart,
  type LucideIcon,
} from "lucide-react"

import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { cn } from "@/lib/utils"
import type { Integration } from "@/lib/integrations"

const iconMap: Record<string, LucideIcon> = {
  Bot,
  Cloud,
  Code,
  Container,
  Database,
  FlaskConical,
  GitBranch,
  Laptop,
  LineChart,
}

function resolveIcon(name: string): LucideIcon {
  return iconMap[name] ?? Cloud
}

export function IntegrationCard({ integration }: { integration: Integration }) {
  const Icon = resolveIcon(integration.icon)
  const isExternal = integration.href.startsWith("http")

  return (
    <Card
      className={cn(
        "relative transition-all duration-(--duration-normal) ease-(--ease-out-app)",
        "shadow-card hover:shadow-card-hover",
        "hover:border-primary/30",
        "focus-within:ring-2 focus-within:ring-ring focus-within:ring-offset-2 focus-within:ring-offset-background"
      )}
    >
      <CardHeader>
        <div
          aria-hidden="true"
          className="mb-2 inline-flex h-10 w-10 items-center justify-center rounded-md bg-primary/10 text-primary"
        >
          <Icon size={20} strokeWidth={2} />
        </div>
        <CardTitle>{integration.name}</CardTitle>
        <CardDescription>{integration.description}</CardDescription>
      </CardHeader>
      <CardContent>
        <Link
          href={integration.href}
          {...(isExternal ? { target: "_blank", rel: "noopener noreferrer" } : {})}
          className={cn(
            "inline-flex items-center gap-1 text-xs font-medium text-primary",
            "transition-colors duration-(--duration-fast)",
            "hover:text-primary-hover",
            "rounded-sm outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-card",
            // Stretch the link to cover the full card for easier click targets
            "after:absolute after:inset-0"
          )}
        >
          <span>View docs</span>
          <ArrowUpRight aria-hidden="true" size={14} strokeWidth={2} />
        </Link>
      </CardContent>
    </Card>
  )
}
