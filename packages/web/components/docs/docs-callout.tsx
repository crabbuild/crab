import {
  AlertCircle,
  AlertTriangle,
  Info,
  Lightbulb,
  Sparkles,
  type LucideIcon,
} from "lucide-react"

import {
  Alert,
  AlertDescription,
  AlertTitle,
} from "@/components/ui/alert"
import { cn } from "@/lib/utils"

export interface DocsCalloutProps {
  type: "tip" | "warning" | "note" | "danger" | "preview"
  title?: string
  children: React.ReactNode
}

const calloutConfig: Record<
  DocsCalloutProps["type"],
  { icon: LucideIcon; variant: "default" | "destructive"; className?: string }
> = {
  tip: {
    icon: Lightbulb,
    variant: "default",
    className: "border-primary/30",
  },
  warning: {
    icon: AlertTriangle,
    variant: "destructive",
  },
  note: {
    icon: Info,
    variant: "default",
  },
  danger: {
    icon: AlertCircle,
    variant: "destructive",
  },
  preview: {
    icon: Sparkles,
    variant: "default",
    className:
      "border-amber-200 bg-amber-50/80 text-amber-950 *:data-[slot=alert-description]:text-amber-900/80 [&>svg]:text-amber-600 dark:border-amber-800/70 dark:bg-amber-950/30 dark:text-amber-100 dark:*:data-[slot=alert-description]:text-amber-200/80 dark:[&>svg]:text-amber-400",
  },
}

export function DocsCallout({ type, title, children }: DocsCalloutProps) {
  const { icon: Icon, variant, className } = calloutConfig[type]

  return (
    <Alert variant={variant} className={cn(className)}>
      <Icon />
      {title && <AlertTitle>{title}</AlertTitle>}
      <AlertDescription>{children}</AlertDescription>
    </Alert>
  )
}
