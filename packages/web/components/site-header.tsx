import Link from "next/link"
import { ThemeToggle } from "./theme-toggle"
import { CrabLogo } from "./crab-logo"
import { MegaMenu } from "@/components/navigation/mega-menu"
import { MobileNav } from "@/components/navigation/mobile-nav"

export function SiteHeader() {
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

        {/* Right: theme toggle + mobile hamburger */}
        <div className="flex items-center gap-2">
          <ThemeToggle />
          {/* Mobile hamburger — hidden at md and above */}
          <MobileNav />
        </div>
      </div>
    </header>
  )
}
