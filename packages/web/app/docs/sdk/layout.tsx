import { DocsLayout } from "fumadocs-ui/layouts/docs"

import { CrabLogo } from "@/components/crab-logo"
import { sdkSource } from "@/lib/source"

export default function SdkDocsLayout({
  children,
}: {
  children: React.ReactNode
}) {
  return (
    <DocsLayout
      tree={sdkSource.pageTree}
      nav={{
        title: (
          <span className="flex items-center gap-2">
            <CrabLogo size={20} /> Crab SDK
          </span>
        ),
      }}
    >
      {children}
    </DocsLayout>
  )
}
