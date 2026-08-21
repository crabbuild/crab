import type { Metadata } from "next"
import { Geist, Geist_Mono, Inter } from "next/font/google"

import "./globals.css"
import { ThemeProvider } from "@/components/theme-provider"
import { RootProvider } from "fumadocs-ui/provider/next"
import { SkipNavLink } from "@/components/navigation/skip-nav-link"
import { cn } from "@/lib/utils";

export const metadata: Metadata = {
  metadataBase: new URL('https://crab.build'),
  title: {
    default: 'Crab — Serverless Git Remote',
    template: '%s | Crab',
  },
  description: 'Git remote helper for cloud object storage. No servers, no LFS endpoints.',
  icons: {
    icon: { url: '/crab.optimized.svg', type: 'image/svg+xml' },
    apple: '/apple-icon.png',
  },
  openGraph: {
    type: 'website',
    siteName: 'Crab',
    images: [{ url: '/og-image.png', width: 1200, height: 630, alt: 'Crab — Serverless Git Remote' }],
  },
}

const inter = Inter({subsets:['latin'],variable:'--font-sans'})

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
      className={cn("antialiased", fontMono.variable, "font-sans", inter.variable)}
    >
      <body>
        <SkipNavLink />
        <RootProvider>
          <ThemeProvider>{children}</ThemeProvider>
        </RootProvider>
      </body>
    </html>
  )
}
