import Link from "next/link"
import { ThemeToggle } from "./theme-toggle"
import { CrabLogo } from "./crab-logo"
import { MegaMenu } from "@/components/navigation/mega-menu"
import { MobileNav } from "@/components/navigation/mobile-nav"

async function starCount(): Promise<number | null> {
  try {
    const response = await fetch(
      "https://api.github.com/repos/crabbuild/crab/stargazers/count",
      {
        headers: {
          Accept: "application/vnd.github+json",
          "X-GitHub-Api-Version": "2026-03-10",
        },
        next: { revalidate: 3600 },
      }
    )

    if (!response.ok) return null

    const repository: unknown = await response.json()
    if (
      typeof repository !== "object" ||
      repository === null ||
      !("count" in repository) ||
      typeof repository.count !== "number"
    ) {
      return null
    }

    return repository.count
  } catch {
    // The repository link must remain usable when GitHub is unavailable.
    return null
  }
}

export async function SiteHeader() {
  const stars = await starCount()

  // Border and background are intentionally omitted here so the header can
  // be wrapped in `<ScrollHeader>` on marketing pages (transparent at top,
  // solid once the visitor scrolls). Layouts that want a permanently solid
  // header should provide their own surrounding background.
  return (
    <header>
      <div className="mx-auto flex h-16 max-w-7xl items-center justify-between px-4 sm:px-6 lg:px-8">
        {/* Left: logo + wide-screen nav */}
        <div className="flex items-center gap-6">
          <Link
            href="/"
            className="flex items-center gap-2 text-sm font-semibold text-foreground"
          >
            <CrabLogo size={28} />
            Crab
          </Link>
          {/* Wide-screen navigation — hidden below md */}
          <MegaMenu />
        </div>

        {/* Right: external links + theme toggle + mobile hamburger */}
        <div className="flex items-center gap-2">
          <a
            href="https://github.com/crabbuild/crab"
            target="_blank"
            rel="noopener noreferrer"
            aria-label={
              stars === null
                ? "Crab on GitHub"
                : `Crab on GitHub, ${stars.toLocaleString("en-US")} stars`
            }
            className="inline-flex items-center gap-1.5 rounded-md p-2 hover:bg-neutral-100 dark:hover:bg-neutral-800"
          >
            <svg
              xmlns="http://www.w3.org/2000/svg"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth={2}
              strokeLinecap="round"
              strokeLinejoin="round"
              className="h-5 w-5"
              aria-hidden="true"
            >
              <path d="M15 22v-4a4.8 4.8 0 0 0-1-3.5c3 0 6-2 6-5.5.08-1.25-.27-2.48-1-3.5.28-1.15.28-2.35 0-3.5 0 0-1 0-3 1.5-2.64-.5-5.36-.5-8 0C6 2 5 2 5 2c-.3 1.15-.3 2.35 0 3.5A5.403 5.403 0 0 0 4 9c0 3.5 3 5.5 6 5.5-.39.49-.68 1.05-.85 1.65-.17.6-.22 1.23-.15 1.85v4" />
              <path d="M9 18c-4.51 2-5-2-7-2" />
            </svg>
            {stars !== null && (
              <span className="text-xs font-medium tabular-nums">
                {stars.toLocaleString("en-US")}
              </span>
            )}
          </a>
          <ThemeToggle />
          {/* Mobile hamburger — hidden at md and above */}
          <MobileNav />
        </div>
      </div>
    </header>
  )
}
