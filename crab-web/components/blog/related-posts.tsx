import type { LucideIcon } from "lucide-react"
import Link from "next/link"

import { Card, CardHeader, CardTitle } from "@/components/ui/card"
import { cn } from "@/lib/utils"

export interface RelatedPost {
  title: string
  slug: string
  categoryIcon: LucideIcon
  categoryLabel: string
}

export interface RelatedPostsProps {
  posts: RelatedPost[]
  className?: string
}

export function RelatedPosts({ posts, className }: RelatedPostsProps) {
  if (posts.length === 0) return null

  return (
    <section className={cn("space-y-3", className)}>
      <h3 className="text-sm font-semibold text-foreground">Related Posts</h3>
      <div className="grid gap-2">
        {posts.map((post) => {
          const Icon = post.categoryIcon
          return (
            <Link key={post.slug} href={`/blog/${post.slug}`} className="block">
              <Card
                size="sm"
                className="transition-all duration-200 hover:shadow-md hover:ring-primary/20"
              >
                <CardHeader>
                  <div className="flex items-center gap-2">
                    <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md bg-primary/10 text-primary">
                      <Icon size={14} strokeWidth={2} />
                    </span>
                    <CardTitle className="line-clamp-1 text-xs font-medium">
                      {post.title}
                    </CardTitle>
                  </div>
                </CardHeader>
              </Card>
            </Link>
          )
        })}
      </div>
    </section>
  )
}
