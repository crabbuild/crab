import type { Metadata } from "next"
import { notFound, permanentRedirect } from "next/navigation"
import { DocsBody, DocsPage } from "fumadocs-ui/page"

import { StructuredData } from "@/components/structured-data"
import { createPageMetadata } from "@/lib/metadata"
import { sdkSource } from "@/lib/source"
import { createBreadcrumbStructuredData } from "@/lib/structured-data"
import { getMDXComponents } from "@/mdx-components"

export default async function SdkDocPage({
  params,
}: {
  params: Promise<{ slug?: string[] }>
}) {
  const { slug } = await params
  if (!slug || slug.length === 0) {
    permanentRedirect("/docs/sdk/getting-started")
  }
  const page = sdkSource.getPage(slug)
  if (!page) notFound()
  const MDX = page.data.body
  const structuredData = createBreadcrumbStructuredData([
    { name: "Documentation", path: "/docs" },
    { name: "Rust SDK", path: "/docs/sdk" },
    { name: page.data.title ?? "Crab Rust SDK" },
  ])

  return (
    <>
      <StructuredData data={structuredData} />
      <DocsPage toc={page.data.toc} id="main-content">
        <DocsBody>
          <MDX components={getMDXComponents({})} />
        </DocsBody>
      </DocsPage>
    </>
  )
}

export function generateStaticParams() {
  return sdkSource.generateParams()
}

export async function generateMetadata({
  params,
}: {
  params: Promise<{ slug?: string[] }>
}): Promise<Metadata> {
  const { slug } = await params
  const page = sdkSource.getPage(slug)
  if (!page) return {}

  return createPageMetadata({
    title: page.data.title ?? "Crab Rust SDK",
    description: page.data.description ?? "Crab Rust SDK documentation.",
    path: `/docs/sdk/${page.slugs.join("/")}`,
  })
}
