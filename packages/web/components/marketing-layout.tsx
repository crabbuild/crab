import { ScrollHeader } from "./navigation/scroll-header"
import { SiteFooter } from "./site-footer"
import { SiteHeader } from "./site-header"

export function MarketingLayout({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex min-h-svh flex-col">
      {/* Marketing pages get a transparent-at-top header that transitions
          to a solid background once the visitor scrolls past the hero.
          Docs pages use Fumadocs' DocsLayout instead and keep their
          permanently solid header. */}
      <ScrollHeader>
        <SiteHeader />
      </ScrollHeader>
      <main id="main-content" className="flex-1">{children}</main>
      <SiteFooter />
    </div>
  )
}
