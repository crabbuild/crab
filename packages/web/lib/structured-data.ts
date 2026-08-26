import { SITE_URL } from "@/lib/metadata"

export type StructuredDataValue = Record<string, unknown>

interface BreadcrumbItem {
  name: string
  path?: string
}

interface ArticleStructuredDataOptions {
  type: "Article" | "BlogPosting"
  title: string
  description: string
  path: string
  imagePath: string
  publishedTime: string
  author: string
  section: string
  tags: string[]
  breadcrumbs: BreadcrumbItem[]
}

function absoluteUrl(path: string): string {
  return new URL(path, SITE_URL).toString()
}

function breadcrumbList(items: BreadcrumbItem[]): StructuredDataValue {
  return {
    "@type": "BreadcrumbList",
    itemListElement: items.map((item, index) => ({
      "@type": "ListItem",
      position: index + 1,
      name: item.name,
      ...(item.path ? { item: absoluteUrl(item.path) } : {}),
    })),
  }
}

export function createBreadcrumbStructuredData(
  items: BreadcrumbItem[]
): StructuredDataValue {
  return {
    "@context": "https://schema.org",
    ...breadcrumbList(items),
  }
}

export function createArticleStructuredData({
  type,
  title,
  description,
  path,
  imagePath,
  publishedTime,
  author,
  section,
  tags,
  breadcrumbs,
}: ArticleStructuredDataOptions): StructuredDataValue {
  const url = absoluteUrl(path)

  return {
    "@context": "https://schema.org",
    "@graph": [
      {
        "@type": type,
        "@id": `${url}#article`,
        headline: title,
        description,
        image: [absoluteUrl(imagePath)],
        datePublished: new Date(publishedTime).toISOString(),
        author: {
          "@type": "Organization",
          name: author,
          ...(author === "Crab Team" ? { url: absoluteUrl("/about-us") } : {}),
        },
        publisher: { "@id": `${SITE_URL}/#organization` },
        mainEntityOfPage: { "@type": "WebPage", "@id": url },
        articleSection: section,
        keywords: tags,
        inLanguage: "en-US",
      },
      breadcrumbList(breadcrumbs),
    ],
  }
}

export function serializeStructuredData(data: StructuredDataValue): string {
  return JSON.stringify(data).replace(/</g, "\\u003c")
}
