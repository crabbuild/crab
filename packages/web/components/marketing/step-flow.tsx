import type { LucideIcon } from "lucide-react"

import { Badge } from "@/components/ui/badge"
import {
  Card,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"

export interface StepFlowProps {
  steps: Array<{
    icon: LucideIcon
    title: string
    description: string
  }>
}

export function StepFlow({ steps }: StepFlowProps) {
  return (
    <div className="grid grid-cols-1 gap-8 md:auto-cols-fr md:grid-flow-col">
      {steps.map((step, index) => {
        const Icon = step.icon
        const isLast = index === steps.length - 1

        return (
          <div key={index} className="relative flex flex-col items-center">
            {/* Wide-screen horizontal connector */}
            {!isLast && (
              <div
                aria-hidden="true"
                className="absolute top-12 right-0 hidden h-px w-8 translate-x-full bg-border md:block"
              />
            )}

            {/* Mobile vertical connector */}
            {!isLast && (
              <div
                aria-hidden="true"
                className="absolute bottom-0 left-1/2 h-8 w-px translate-y-full -translate-x-1/2 bg-border md:hidden"
              />
            )}

            <Card className="w-full text-center">
              <CardHeader className="items-center">
                <Badge variant="outline" className="mb-2">
                  {index + 1}
                </Badge>
                <div
                  aria-hidden="true"
                  className="mb-3 inline-flex h-10 w-10 items-center justify-center rounded-md bg-primary/10 text-primary"
                >
                  <Icon size={20} strokeWidth={2} />
                </div>
                <CardTitle>{step.title}</CardTitle>
                <CardDescription>{step.description}</CardDescription>
              </CardHeader>
            </Card>
          </div>
        )
      })}
    </div>
  )
}
