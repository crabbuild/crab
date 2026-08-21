"use client"

import { useState, useMemo } from "react"
import { useSearchParams, useRouter } from "next/navigation"
import { Search, Tag, X } from "lucide-react"

import { cn } from "@/lib/utils"
import {
  BLOG_LEARNING_PATHS,
  BLOG_LEVELS,
  filterByCategory,
  filterByLevel,
  filterByPath,
  filterByTag,
  searchPosts,
  type BlogPostMeta,
} from "@/lib/blog"

interface BlogCategoryFilterProps {
  categories: string[]
  posts: BlogPostMeta[]
  children: (filteredPosts: BlogPostMeta[]) => React.ReactNode
}

export function BlogCategoryFilter({
  categories,
  posts,
  children,
}: BlogCategoryFilterProps) {
  const searchParams = useSearchParams()
  const router = useRouter()

  const activeCategory = searchParams.get("category") ?? "All"
  const activeTag = searchParams.get("tag") ?? ""
  const activePath = searchParams.get("path") ?? "All"
  const activeLevel = searchParams.get("level") ?? "All"
  const [query, setQuery] = useState("")

  const filteredPosts = useMemo(() => {
    let result = filterByPath(activePath, posts)
    result = filterByLevel(activeLevel, result)
    result = filterByCategory(activeCategory, result)
    result = filterByTag(activeTag || undefined, result)
    return searchPosts(query, result)
  }, [activeCategory, activeLevel, activePath, activeTag, query, posts])

  // Collect all unique tags from posts
  const allTags = useMemo(() => {
    const tagSet = new Set<string>()
    posts.forEach((post) => post.tags.forEach((t) => tagSet.add(t)))
    return Array.from(tagSet).sort()
  }, [posts])

  const allCategories = ["All", ...categories]

  function updateParams(
    category: string,
    tag: string,
    path: string,
    level: string
  ) {
    const params = new URLSearchParams()
    if (path !== "All") params.set("path", path)
    if (level !== "All") params.set("level", level)
    if (category !== "All") params.set("category", category)
    if (tag) params.set("tag", tag)
    const qs = params.toString()
    router.replace(qs ? `/blog?${qs}` : "/blog", { scroll: false })
  }

  function handleCategoryChange(category: string) {
    updateParams(category, activeTag, activePath, activeLevel)
  }

  function handlePathChange(path: string) {
    updateParams(activeCategory, activeTag, path, activeLevel)
  }

  function handleLevelChange(level: string) {
    updateParams(activeCategory, activeTag, activePath, level)
  }

  function handleTagChange(tag: string) {
    const newTag = tag === activeTag ? "" : tag
    updateParams(activeCategory, newTag, activePath, activeLevel)
  }

  function clearTag() {
    updateParams(activeCategory, "", activePath, activeLevel)
  }

  return (
    <div className="space-y-8">
      <div className="grid gap-5 rounded-lg border border-border bg-card p-4 shadow-sm lg:grid-cols-[minmax(0,1fr)_18rem] lg:p-5">
        <div className="space-y-5">
          <FilterGroup label="Learning path">
            <FilterButton
              active={activePath === "All"}
              onClick={() => handlePathChange("All")}
            >
              All paths
            </FilterButton>
            {BLOG_LEARNING_PATHS.map((path) => (
              <FilterButton
                key={path.key}
                active={activePath === path.key}
                onClick={() => handlePathChange(path.key)}
              >
                {path.shortLabel}
              </FilterButton>
            ))}
          </FilterGroup>

          <FilterGroup label="Depth">
            <FilterButton
              active={activeLevel === "All"}
              onClick={() => handleLevelChange("All")}
            >
              Any depth
            </FilterButton>
            {BLOG_LEVELS.map((level) => (
              <FilterButton
                key={level}
                active={activeLevel === level}
                onClick={() => handleLevelChange(level)}
              >
                {level}
              </FilterButton>
            ))}
          </FilterGroup>

          <FilterGroup label="Category">
            {allCategories.map((category) => (
              <FilterButton
                key={category}
                active={activeCategory === category}
                onClick={() => handleCategoryChange(category)}
              >
                {category}
              </FilterButton>
            ))}
          </FilterGroup>
        </div>

        <div className="space-y-4">
          <div className="relative">
            <Search
              className="absolute top-1/2 left-3 -translate-y-1/2 text-muted-foreground"
              size={16}
            />
            <input
              type="search"
              placeholder="Search concepts, posts, tags..."
              aria-label="Search blog posts"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              className={cn(
                "w-full rounded-md border border-border bg-background py-2.5 pr-4 pl-9 text-sm",
                "placeholder:text-muted-foreground",
                "focus:ring-2 focus:ring-ring focus:outline-none"
              )}
            />
          </div>

          <div className="text-xs text-muted-foreground">
            Showing{" "}
            <span className="font-medium text-foreground">
              {filteredPosts.length}
            </span>{" "}
            of {posts.length} posts
          </div>
        </div>
      </div>

      {activeTag && (
        <div className="flex items-center gap-2">
          <Tag size={14} className="text-muted-foreground" />
          <span className="text-sm text-muted-foreground">
            Filtered by tag:
          </span>
          <button
            onClick={clearTag}
            className="inline-flex items-center gap-1 rounded-full bg-primary/10 px-3 py-1 text-xs font-medium text-primary transition-colors hover:bg-primary/20"
          >
            {activeTag}
            <X size={12} />
          </button>
        </div>
      )}

      {!activeTag && allTags.length > 0 && (
        <div className="flex flex-wrap items-center gap-1.5">
          <Tag size={14} className="mr-1 text-muted-foreground" />
          {allTags.map((tag) => (
            <button
              key={tag}
              onClick={() => handleTagChange(tag)}
              className="rounded-full border border-border px-2.5 py-0.5 text-xs text-muted-foreground transition-colors hover:border-primary/50 hover:text-primary"
            >
              {tag}
            </button>
          ))}
        </div>
      )}

      <div
        key={`${activePath}-${activeLevel}-${activeCategory}-${activeTag}`}
        className="animate-in duration-300 fade-in"
      >
        {filteredPosts.length === 0 ? (
          <p className="py-12 text-center text-muted-foreground">
            No posts found matching your filters.
          </p>
        ) : (
          children(filteredPosts)
        )}
      </div>
    </div>
  )
}

function FilterGroup({
  label,
  children,
}: {
  label: string
  children: React.ReactNode
}) {
  return (
    <div className="space-y-2">
      <div className="text-xs font-semibold tracking-wide text-muted-foreground uppercase">
        {label}
      </div>
      <div className="flex flex-wrap items-center gap-2">{children}</div>
    </div>
  )
}

function FilterButton({
  active,
  onClick,
  children,
}: {
  active: boolean
  onClick: () => void
  children: React.ReactNode
}) {
  return (
    <button
      onClick={onClick}
      className={cn(
        "rounded-full px-3 py-1.5 text-xs font-medium transition-colors duration-(--duration-fast) sm:text-sm",
        active
          ? "bg-primary text-primary-foreground"
          : "bg-muted text-muted-foreground hover:bg-muted/80 hover:text-foreground"
      )}
    >
      {children}
    </button>
  )
}
