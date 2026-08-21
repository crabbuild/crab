import { cn } from "@/lib/utils"

interface CrabLogoProps {
  className?: string
  size?: number
}

export function CrabLogo({ className, size = 24 }: CrabLogoProps) {
  return (
    <span
      role="img"
      aria-label="Crab logo"
      className={cn("inline-block shrink-0 bg-current", className)}
      style={{
        width: size,
        height: size,
        mask: "url('/crab.optimized.svg') center / contain no-repeat",
        WebkitMask: "url('/crab.optimized.svg') center / contain no-repeat",
      }}
    />
  )
}
