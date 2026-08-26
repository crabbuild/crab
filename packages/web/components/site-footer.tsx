import Link from "next/link"
import { ArrowUpRight } from "lucide-react"

import { CrabLogo } from "./crab-logo"

const footerGroups = [
  {
    title: "Product",
    links: [
      { label: "CLI", href: "/cli" },
      { label: "Crab Auth Service", href: "/auth" },
      { label: "Crab Cache Service", href: "/cache" },
    ],
  },
  {
    title: "Resources",
    links: [
      { label: "CLI docs", href: "/docs/cli" },
      { label: "Learning Library", href: "/library" },
      { label: "Blog", href: "/blog" },
      { label: "Changelog", href: "/changelog" },
    ],
  },
  {
    title: "Company",
    links: [
      { label: "About us", href: "/about-us" },
      {
        label: "GitHub",
        href: "https://github.com/crabbuild/crab",
        external: true,
      },
      {
        label: "Contact",
        href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog",
        external: true,
      },
    ],
  },
  {
    title: "Legal",
    links: [
      { label: "Privacy", href: "/privacy" },
      { label: "Terms of service", href: "/terms-of-service" },
    ],
  },
]

function SiteFooter() {
  return (
    <footer className="border-t bg-muted/30">
      <div className="mx-auto max-w-7xl px-6 py-12 md:py-14">
        <div className="grid gap-10 lg:grid-cols-[minmax(16rem,1.35fr)_2fr]">
          <div className="max-w-sm">
            <Link
              href="/"
              className="inline-flex items-center gap-2 text-base font-semibold text-foreground"
            >
              <CrabLogo size={28} />
              Crab
            </Link>
            <p className="mt-4 text-sm leading-6 text-muted-foreground">
              Serverless Git for large repositories, backed by the cloud object
              storage your team already controls.
            </p>
            <div className="mt-5 inline-flex rounded-md border bg-background px-3 py-2 text-xs font-medium text-muted-foreground">
              CLI-first. Bucket-owned. Git-shaped.
            </div>
          </div>

          <nav
            aria-label="Footer"
            className="grid grid-cols-2 gap-8 sm:grid-cols-4"
          >
            {footerGroups.map((group) => (
              <div key={group.title}>
                <h2 className="text-xs font-semibold text-foreground">
                  {group.title}
                </h2>
                <ul className="mt-4 space-y-3">
                  {group.links.map((link) => (
                    <li key={link.href}>
                      <FooterLink link={link} />
                    </li>
                  ))}
                </ul>
              </div>
            ))}
          </nav>
        </div>

        <div className="mt-10 flex flex-col gap-3 border-t pt-6 text-sm text-muted-foreground sm:flex-row sm:items-center sm:justify-between">
          <p>&copy; {new Date().getFullYear()} Beyondnote Technology Inc</p>
          <p>Repository content stays in customer-controlled storage.</p>
        </div>
      </div>
    </footer>
  )
}

function FooterLink({
  link,
}: {
  link: { label: string; href: string; external?: boolean }
}) {
  const className =
    "inline-flex items-center gap-1.5 text-sm text-muted-foreground transition-colors hover:text-foreground focus-visible:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/30"

  if (link.external) {
    return (
      <a
        href={link.href}
        target="_blank"
        rel="noopener noreferrer"
        className={className}
      >
        {link.label}
        <ArrowUpRight className="size-3.5" aria-hidden="true" />
      </a>
    )
  }

  return (
    <Link href={link.href} className={className}>
      {link.label}
    </Link>
  )
}

export { SiteFooter }
