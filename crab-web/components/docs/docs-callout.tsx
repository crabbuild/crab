import {
  AlertCircle,
  AlertTriangle,
  Info,
  Lightbulb,
  type LucideIcon,
} from "lucide-react"

import {
  Alert,
  AlertDescription,
  AlertTitle,
} from "@/components/ui/alert"
import { cn } from "@/lib/utils"

export interface DocsCalloutProps {
  type: "tip" | "warning" | "note" | "danger"
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
