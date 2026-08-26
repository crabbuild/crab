"use client"

import Link from "next/link"
import {
  ArrowRight,
  BookOpen,
  GitBranch,
  Package,
  GraduationCap,
  Layers,
  Briefcase,
  Rocket,
  Clock,
  Network,
} from "lucide-react"

import { Badge } from "@/components/ui/badge"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { LibraryGuideFilter } from "@/components/library/library-guide-filter"
import {
  getLibraryPath,
  type LibraryPathKey,
  type LibraryGuideMeta,
} from "@/lib/library"
import { formatBlogDate } from "@/lib/blog-date"
import { cn } from "@/lib/utils"

const categoryIcons: Record<string, typeof Package> = {
  Product: Package,
  Tutorial: GraduationCap,
  Architecture: Layers,
  "Use Case": Briefcase,
  Release: Rocket,
}

const pathIcons: Record<LibraryPathKey, typeof BookOpen> = {
  "start-here": BookOpen,
  "first-workflow": GitBranch,
  "core-internals": Network,
  "advanced-operations": Layers,
}

function PostCard({ post }: { post: LibraryGuideMeta }) {
  const Icon = categoryIcons[post.category] ?? Package
  const learningPath = getLibraryPath(post.path)
  const PathIcon = pathIcons[post.path] ?? BookOpen

  return (
    <Link href={`/library/${post.slug}`} className="block h-full">
      <Card
        className={cn(
          "h-full transition-all duration-(--duration-normal)",
          "hover:shadow-card-hover hover:ring-1 hover:ring-primary/20"
        )}
      >
        <CardHeader className="pb-2">
          <div className="flex flex-wrap items-center gap-2">
            <Badge variant="secondary" className="w-fit gap-1">
              <PathIcon size={12} strokeWidth={2} />
              {learningPath.shortLabel} {post.pathOrder}
            </Badge>
            <Badge variant="outline" className="w-fit gap-1">
              <Icon size={12} strokeWidth={2} />
              {post.category}
            </Badge>
          </div>
          <CardTitle className="mt-2 text-base leading-snug font-semibold">
            {post.title}
          </CardTitle>
        </CardHeader>
        <CardContent className="flex flex-1 flex-col justify-between gap-3">
          <p className="line-clamp-2 text-sm text-muted-foreground">
            {post.description}
          </p>
          <div className="rounded-md bg-muted/50 p-3 text-xs leading-relaxed text-muted-foreground">
            <span className="font-medium text-foreground">Outcome:</span>{" "}
            {post.outcome}
          </div>
          {post.concepts.length > 0 && (
            <div className="flex flex-wrap gap-1">
              {post.concepts.slice(0, 3).map((tag) => (
                <span
                  key={tag}
                  className="rounded-full border border-border px-2 py-0.5 text-[10px] text-muted-foreground"
                >
                  {tag}
                </span>
              ))}
              {post.concepts.length > 3 && (
                <span className="text-[10px] text-muted-foreground">
                  +{post.concepts.length - 3}
                </span>
              )}
            </div>
          )}
          <div className="flex items-center justify-between gap-3 text-xs text-muted-foreground">
            <time dateTime={post.date}>
              {formatBlogDate(post.date, "short")}
            </time>
            <span className="flex items-center gap-1">
              <Clock size={12} />
              {post.readingTimeMinutes} min
            </span>
            <span className="ml-auto rounded-full bg-primary/10 px-2 py-0.5 font-medium text-primary">
              {post.level}
            </span>
          </div>
          <span className="inline-flex items-center gap-1 text-xs font-medium text-primary">
            Read guide
            <ArrowRight size={12} />
          </span>
        </CardContent>
      </Card>
    </Link>
  )
}

interface LibraryIndexContentProps {
  categories: string[]
  posts: LibraryGuideMeta[]
}

export function LibraryIndexContent({
  categories,
  posts,
}: LibraryIndexContentProps) {
  return (
    <LibraryGuideFilter categories={categories} posts={posts}>
      {(filteredPosts) => (
        <section aria-label="Filtered library guides" className="space-y-4">
          <div className="flex items-end justify-between gap-4">
            <div>
              <h2 className="text-xl font-semibold tracking-tight">
                Explore all guides
              </h2>
              <p className="mt-1 text-sm text-muted-foreground">
                Filter by learning path, depth, topic, or the concept you are
                trying to understand.
              </p>
            </div>
          </div>
          <div className="grid gap-5 sm:grid-cols-2 lg:grid-cols-3">
            {filteredPosts.map((post) => (
              <PostCard key={post.slug} post={post} />
            ))}
          </div>
        </section>
      )}
    </LibraryGuideFilter>
  )
}
