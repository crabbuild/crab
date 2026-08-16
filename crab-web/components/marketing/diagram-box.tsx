import { cn } from "@/lib/utils"

export interface DiagramBoxProps {
  children: React.ReactNode
  maxWidth?: number
  className?: string
}

export function DiagramBox({ children, maxWidth, className }: DiagramBoxProps) {
  return (
    <div
      className={cn(
        "bg-muted/50 border border-border rounded-xl p-6 mx-auto w-full",
        className
      )}
      style={maxWidth ? { maxWidth: `${maxWidth}px` } : undefined}
    >
      {children}
    </div>
  )
}
