import type { LucideIcon } from "lucide-react"
import Link from "next/link"

import { Badge } from "@/components/ui/badge"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { cn } from "@/lib/utils"

export interface BlogCardProps {
  title: string
  date: string
  excerpt: string
  categoryIcon: LucideIcon
  categoryLabel: string
  slug: string
}

export function BlogCard({
  title,
  date,
  excerpt,
  categoryIcon: CategoryIcon,
  categoryLabel,
  slug,
}: BlogCardProps) {
  return (
    <Link href={`/blog/${slug}`} className="block">
      <Card
        className={cn(
          "h-full transition-all duration-200 hover:shadow-md hover:ring-primary/20"
        )}
      >
        <CardHeader>
          <Badge variant="secondary" className="w-fit">
            <CategoryIcon size={12} strokeWidth={2} data-icon="inline-start" />
            {categoryLabel}
          </Badge>
          <CardTitle className="mt-2 text-base font-semibold">
            {title}
          </CardTitle>
          <time
            dateTime={date}
            className="text-xs text-muted-foreground"
          >
            {new Date(date).toLocaleDateString("en-US", {
              year: "numeric",
              month: "long",
              day: "numeric",
            })}
          </time>
        </CardHeader>
        <CardContent>
          <p className="line-clamp-3 text-xs text-muted-foreground">
            {excerpt}
          </p>
        </CardContent>
      </Card>
    </Link>
  )
}
