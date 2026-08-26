import { describe, expect, it } from "vitest"

import {
  createArticleStructuredData,
  createBreadcrumbStructuredData,
  serializeStructuredData,
} from "@/lib/structured-data"

describe("structured data", () => {
  it("builds absolute article and breadcrumb references", () => {
    expect(
      createArticleStructuredData({
        type: "BlogPosting",
        title: "Git for large files",
        description: "An article about large-file version control.",
        path: "/blog/git-for-large-files",
        imagePath: "/blog/git-for-large-files/opengraph-image",
        publishedTime: "2026-08-25",
        author: "Crab Team",
        section: "Product",
        tags: ["git", "large-files"],
        breadcrumbs: [
          { name: "Blog", path: "/blog" },
          { name: "Git for large files" },
        ],
      })
    ).toMatchObject({
      "@context": "https://schema.org",
      "@graph": [
        {
          "@type": "BlogPosting",
          "@id": "https://crab.build/blog/git-for-large-files#article",
          image: [
            "https://crab.build/blog/git-for-large-files/opengraph-image",
          ],
          datePublished: "2026-08-25T00:00:00.000Z",
          publisher: { "@id": "https://crab.build/#organization" },
          mainEntityOfPage: {
            "@type": "WebPage",
            "@id": "https://crab.build/blog/git-for-large-files",
          },
        },
        {
          "@type": "BreadcrumbList",
          itemListElement: [
            {
              "@type": "ListItem",
              position: 1,
              name: "Blog",
              item: "https://crab.build/blog",
            },
            {
              "@type": "ListItem",
              position: 2,
              name: "Git for large files",
            },
          ],
        },
      ],
    })
  })

  it("builds standalone breadcrumb data", () => {
    expect(
      createBreadcrumbStructuredData([
        { name: "Documentation", path: "/docs" },
        { name: "Crab clone" },
      ])
    ).toMatchObject({
      "@context": "https://schema.org",
      "@type": "BreadcrumbList",
    })
  })

  it("escapes markup delimiters before embedding JSON-LD", () => {
    expect(serializeStructuredData({ value: "</script>" })).toBe(
      '{"value":"\\u003c/script>"}'
    )
  })
})
