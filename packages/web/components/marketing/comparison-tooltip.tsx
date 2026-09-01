"use client"

import { Tooltip } from "@base-ui/react/tooltip"
import { CircleQuestionMark } from "lucide-react"

export function ComparisonTooltip({ message }: { message: string }) {
  return (
    <Tooltip.Root>
      <Tooltip.Trigger
        delay={0}
        aria-label="More information"
        className="inline-flex size-5 items-center justify-center rounded-full text-muted-foreground transition-colors hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none"
      >
        <CircleQuestionMark className="size-3.5" aria-hidden="true" />
      </Tooltip.Trigger>
      <Tooltip.Portal>
        <Tooltip.Positioner sideOffset={6} className="z-50">
          <Tooltip.Popup className="max-w-64 rounded-md border border-border bg-popover px-3 py-2 text-xs leading-5 text-popover-foreground shadow-md">
            {message}
          </Tooltip.Popup>
        </Tooltip.Positioner>
      </Tooltip.Portal>
    </Tooltip.Root>
  )
}
