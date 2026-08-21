"use client"

import type { ReactNode } from "react"
import { createElement } from "react"
import Link from "next/link"
import { usePathname } from "next/navigation"
import type * as PageTree from "fumadocs-core/page-tree"
import type { SidebarPageTreeComponents } from "fumadocs-ui/components/sidebar/page-tree"

import { getSidebarIcon } from "@/components/docs/docs-sidebar-icons"
import { cn } from "@/lib/utils"

/**
 * Extracts a slug key from a page URL for icon lookup.
 * Given a URL like "/docs/cli/commands/push", returns "commands/push".
 * For top-level items like "/docs/cli/getting-started", returns "getting-started".
 */
function extractSlugFromUrl(url: string): string {
  // Remove the /docs/cli prefix
  const match = url.match(/^\/docs\/cli\/(.+)$/)
  return match ? match[1] : ""
}

/**
 * Extracts a slug key from a folder item for icon lookup.
 * Uses the folder's index URL or infers from the folder name.
 */
function extractSlugFromFolder(item: PageTree.Folder): string {
  if (item.index) {
    return extractSlugFromUrl(item.index.url)
  }
  // Fallback: use the $id or name as slug
  if (item.$id) {
    // $id often contains the path segments
    const parts = item.$id.split(":")
    const last = parts[parts.length - 1]
    if (last) return last
  }
  return ""
}

/**
 * Custom sidebar Item component that renders a Lucide icon
 * alongside each navigation item.
 */
function SidebarItem({ item }: { item: PageTree.Item }) {
  const pathname = usePathname()
  const slug = extractSlugFromUrl(item.url)
  const Icon = getSidebarIcon(slug)
  const isActive = pathname === item.url

  return (
    <Link
      href={item.url}
      data-active={isActive}
      className={cn(
        "relative flex flex-row items-center gap-2 rounded-lg p-2 text-start text-fd-muted-foreground transition-colors",
        "hover:bg-fd-accent/50 hover:text-fd-accent-foreground/80 hover:transition-none",
        "data-[active=true]:bg-fd-primary/10 data-[active=true]:text-fd-primary"
      )}
    >
      {Icon && createElement(Icon, { className: "size-4 shrink-0" })}
      {!Icon && item.icon}
      <span>{item.name}</span>
    </Link>
  )
}

/**
 * Custom sidebar Folder component that renders a Lucide icon
 * alongside each folder heading.
 */
function SidebarFolder({
  item,
  children,
}: {
  item: PageTree.Folder
  children: ReactNode
}) {
  const pathname = usePathname()
  const slug = extractSlugFromFolder(item)
  const Icon = getSidebarIcon(slug)
  const isActive = item.index ? pathname === item.index.url : false

  return (
    <div className="flex flex-col">
      {item.index ? (
        <Link
          href={item.index.url}
          data-active={isActive}
          className={cn(
            "relative flex flex-row items-center gap-2 rounded-lg p-2 text-start font-medium text-fd-muted-foreground transition-colors",
            "hover:bg-fd-accent/50 hover:text-fd-accent-foreground/80 hover:transition-none",
            "data-[active=true]:bg-fd-primary/10 data-[active=true]:text-fd-primary"
          )}
        >
          {Icon && createElement(Icon, { className: "size-4 shrink-0" })}
          {!Icon && item.icon}
          <span>{item.name}</span>
        </Link>
      ) : (
        <div className="flex flex-row items-center gap-2 p-2 text-start font-medium text-fd-foreground">
          {Icon && createElement(Icon, { className: "size-4 shrink-0" })}
          {!Icon && item.icon}
          <span>{item.name}</span>
        </div>
      )}
      <div className="ms-4 border-s border-fd-border ps-2">{children}</div>
    </div>
  )
}

/**
 * Custom sidebar page tree components that integrate icons from
 * the docs-sidebar-icons mapping.
 */
export const docsSidebarComponents: Partial<SidebarPageTreeComponents> = {
  Item: SidebarItem,
  Folder: SidebarFolder,
}
