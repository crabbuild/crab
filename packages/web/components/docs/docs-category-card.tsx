import type { LucideIcon } from "lucide-react"
import Link from "next/link"

import {
  Card,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { cn } from "@/lib/utils"

export interface DocsCategoryCardProps {
  icon: LucideIcon
  name: string
  description: string
  href: string
  className?: string
}

export function DocsCategoryCard({
  icon: Icon,
  name,
  description,
  href,
  className,
}: DocsCategoryCardProps) {
  return (
    <Link href={href} className="block">
      <Card
        className={cn(
          "h-full transition-shadow duration-200 hover:ring-primary/30",
          className
        )}
      >
        <CardHeader>
          <div
            aria-hidden="true"
            className="mb-3 inline-flex h-10 w-10 items-center justify-center rounded-md bg-primary/10 text-primary"
          >
            <Icon size={20} strokeWidth={2} />
          </div>
          <CardTitle>{name}</CardTitle>
          <CardDescription>{description}</CardDescription>
        </CardHeader>
      </Card>
    </Link>
  )
}
