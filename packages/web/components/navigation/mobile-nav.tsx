"use client"

import { useEffect, useRef, useState } from "react"
import { usePathname } from "next/navigation"
import Link from "next/link"
import {
  Terminal,
  HardDrive,
  ShieldCheck,
  Menu,
  X,
  ChevronRight,
} from "lucide-react"
import { cn } from "@/lib/utils"
import { CrabLogo } from "@/components/crab-logo"

const productNavItems = [
  {
    icon: Terminal,
    name: "CLI",
    description: "Git remote helper for cloud storage",
    href: "/cli",
  },
  {
    icon: ShieldCheck,
    name: "Crab Auth Service",
    description: "Cloud-native authentication",
    href: "/auth",
  },
  {
    icon: HardDrive,
    name: "Crab Cache Service",
    description: "Local and remote caching layer",
    href: "/cache",
  },
]

const topLevelLinks = [
  { name: "Docs", href: "/docs" },
  { name: "Library", href: "/library" },
  { name: "Use Cases", href: "/use-cases" },
  { name: "Blog", href: "/blog" },
  { name: "Pricing", href: "/pricing" },
]

/**
 * Mobile navigation — hamburger button + full-screen slide-over panel.
 * Inspired by Cloudflare's mobile nav: the panel slides in from the right,
 * covers the full viewport, and has its own close button in the top-right.
 *
 * Only renders on screens below the `md` breakpoint (< 768px).
 */
export function MobileNav() {
  const [open, setOpen] = useState(false)
  const [productsExpanded, setProductsExpanded] = useState(false)
  const pathname = usePathname()
  const prevPathname = useRef(pathname)

  // Close the menu on route change
  useEffect(() => {
    if (prevPathname.current !== pathname) {
      prevPathname.current = pathname
      setOpen(false)
      setProductsExpanded(false)
    }
  }, [pathname])

  // Lock body scroll when menu is open
  useEffect(() => {
    if (open) {
      document.body.style.overflow = "hidden"
    } else {
      document.body.style.overflow = ""
    }
    return () => {
      document.body.style.overflow = ""
    }
  }, [open])

  return (
    <>
      {/* Hamburger button — visible only below md */}
      <button
        type="button"
        onClick={() => setOpen(true)}
        aria-label="Open navigation menu"
        className="inline-flex h-9 w-9 items-center justify-center rounded-md text-foreground hover:bg-muted focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none md:hidden"
      >
        <Menu className="h-5 w-5" />
      </button>

      {/* Full-screen overlay panel */}
      <div
        className={cn(
          "fixed inset-0 z-100 overflow-hidden md:hidden",
          open ? "visible" : "pointer-events-none invisible"
        )}
        aria-modal={open}
        role="dialog"
        aria-label="Navigation menu"
      >
        {/* Backdrop */}
        <div
          className={cn(
            "absolute inset-0 bg-black/40 backdrop-blur-sm transition-opacity duration-300",
            open ? "opacity-100" : "opacity-0"
          )}
          onClick={() => setOpen(false)}
          aria-hidden="true"
        />

        {/* Slide-in panel from right */}
        <nav
          className={cn(
            "absolute top-0 right-0 bottom-0 w-full max-w-sm bg-background shadow-xl",
            "flex flex-col overflow-y-auto",
            "transition-transform duration-300 ease-out",
            open ? "translate-x-0" : "translate-x-full"
          )}
        >
          {/* Panel header with branding + close button */}
          <div className="flex h-16 items-center justify-between border-b border-border px-5">
            <Link
              href="/"
              className="flex items-center gap-2"
              onClick={() => setOpen(false)}
            >
              <CrabLogo size={24} />
              <span className="text-sm font-semibold text-foreground">
                Crab
              </span>
            </Link>
            <button
              type="button"
              onClick={() => setOpen(false)}
              aria-label="Close navigation menu"
              className="inline-flex h-9 w-9 items-center justify-center rounded-md text-foreground hover:bg-muted focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none"
            >
              <X className="h-5 w-5" />
            </button>
          </div>

          {/* Nav links */}
          <div className="flex-1 px-4 py-4">
            {/* Products section with expand/collapse */}
            <div className="mb-1">
              <button
                type="button"
                onClick={() => setProductsExpanded((prev) => !prev)}
                className="flex min-h-[44px] w-full items-center justify-between rounded-lg px-3 py-3 text-sm font-medium text-foreground hover:bg-muted"
                aria-expanded={productsExpanded}
              >
                Products
                <ChevronRight
                  className={cn(
                    "h-4 w-4 text-muted-foreground transition-transform duration-200",
                    productsExpanded && "rotate-90"
                  )}
                />
              </button>

              <div
                className={cn(
                  "overflow-hidden transition-[max-height,opacity] duration-200 ease-out",
                  productsExpanded
                    ? "max-h-[500px] opacity-100"
                    : "max-h-0 opacity-0"
                )}
              >
                <ul className="flex flex-col gap-0.5 pt-1 pb-2 pl-2">
                  {productNavItems.map((item) => (
                    <li key={item.href}>
                      <Link
                        href={item.href}
                        className="flex min-h-[44px] items-center gap-3 rounded-lg px-3 py-2.5 text-sm hover:bg-muted"
                      >
                        <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-md bg-muted">
                          <item.icon className="h-4 w-4 text-muted-foreground" />
                        </span>
                        <span className="flex flex-col">
                          <span className="font-medium text-foreground">
                            {item.name}
                          </span>
                          <span className="text-xs text-muted-foreground">
                            {item.description}
                          </span>
                        </span>
                      </Link>
                    </li>
                  ))}
                </ul>
              </div>
            </div>

            {/* Top-level links */}
            <ul className="flex flex-col gap-0.5">
              {topLevelLinks.map((link) => (
                <li key={link.href}>
                  <Link
                    href={link.href}
                    className="flex min-h-[44px] items-center rounded-lg px-3 py-3 text-sm font-medium text-foreground hover:bg-muted"
                  >
                    {link.name}
                  </Link>
                </li>
              ))}
            </ul>
          </div>
        </nav>
      </div>
    </>
  )
}
