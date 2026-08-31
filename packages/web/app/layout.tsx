import type { Metadata } from "next"
import { Geist_Mono, Inter } from "next/font/google"

import "./globals.css"
import { ThemeProvider } from "@/components/theme-provider"
import { RootProvider } from "fumadocs-ui/provider/next"
import { SkipNavLink } from "@/components/navigation/skip-nav-link"
import { StructuredData } from "@/components/structured-data"
import {
  CRAB_LOGO_PATH,
  SITE_DESCRIPTION,
  SITE_NAME,
  SITE_URL,
  createPageMetadata,
} from "@/lib/metadata"
import { cn } from "@/lib/utils"

const rootPageMetadata = createPageMetadata({
  title: "Crab — Serverless Git Remote",
  description: SITE_DESCRIPTION,
  path: "/",
  absoluteTitle: true,
})

export const metadata: Metadata = {
  ...rootPageMetadata,
  metadataBase: new URL(SITE_URL),
  applicationName: SITE_NAME,
  title: {
    default: "Crab — Serverless Git Remote",
    template: `%s | ${SITE_NAME}`,
  },
  keywords: [
    "serverless Git",
    "Git remote helper",
    "large file version control",
    "cloud object storage",
    "S3 Git storage",
    "GCS Git storage",
    "Azure Blob Git storage",
    "Git LFS alternative",
    "Xet deduplication",
  ],
  authors: [{ name: "Beyondnote Technology Inc", url: "/about-us" }],
  creator: "Beyondnote Technology Inc",
  publisher: "Beyondnote Technology Inc",
  category: "technology",
  referrer: "origin-when-cross-origin",
  formatDetection: {
    address: false,
    email: false,
    telephone: false,
  },
  icons: {
    icon: [{ url: CRAB_LOGO_PATH, type: "image/svg+xml", sizes: "any" }],
    shortcut: [{ url: CRAB_LOGO_PATH, type: "image/svg+xml" }],
    apple: [{ url: "/apple-icon.png", sizes: "180x180", type: "image/png" }],
  },
  robots: {
    index: true,
    follow: true,
    googleBot: {
      index: true,
      follow: true,
      "max-image-preview": "large",
      "max-snippet": -1,
      "max-video-preview": -1,
    },
  },
}

const structuredData = {
  "@context": "https://schema.org",
  "@graph": [
    {
      "@type": "Organization",
      "@id": `${SITE_URL}/#organization`,
      name: SITE_NAME,
      legalName: "Beyondnote Technology Inc",
      url: SITE_URL,
      logo: {
        "@type": "ImageObject",
        url: `${SITE_URL}${CRAB_LOGO_PATH}`,
        width: 400,
        height: 400,
      },
      sameAs: ["https://github.com/crabbuild/crab"],
    },
    {
      "@type": "WebSite",
      "@id": `${SITE_URL}/#website`,
      url: SITE_URL,
      name: SITE_NAME,
      description: SITE_DESCRIPTION,
      inLanguage: "en",
      publisher: { "@id": `${SITE_URL}/#organization` },
    },
  ],
}

const inter = Inter({ subsets: ["latin"], variable: "--font-sans" })

const fontMono = Geist_Mono({
  subsets: ["latin"],
  variable: "--font-mono",
})

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode
}>) {
  return (
    <html
      lang="en"
      suppressHydrationWarning
      className={cn(
        "antialiased",
        fontMono.variable,
        "font-sans",
        inter.variable
      )}
    >
      <body>
        <StructuredData data={structuredData} />
        <SkipNavLink />
        <RootProvider>
          <ThemeProvider>{children}</ThemeProvider>
        </RootProvider>
      </body>
    </html>
  )
}
