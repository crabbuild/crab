import type { MetadataRoute } from "next"
import { blogSource } from "@/lib/blog-source"
import { cliSource } from "@/lib/source"

const BASE_URL = "https://crab.build"

export default function sitemap(): MetadataRoute.Sitemap {
  const staticRoutes: MetadataRoute.Sitemap = [
    { url: `${BASE_URL}/` },
    { url: `${BASE_URL}/about-us` },
    { url: `${BASE_URL}/cli` },
    { url: `${BASE_URL}/use-cases` },
    { url: `${BASE_URL}/pricing` },
    { url: `${BASE_URL}/blog` },
    { url: `${BASE_URL}/changelog` },
    { url: `${BASE_URL}/integrations` },
    { url: `${BASE_URL}/privacy` },
    { url: `${BASE_URL}/terms-of-service` },
  ]

  const blogEntries: MetadataRoute.Sitemap = blogSource
    .getPages()
    .map((post) => ({
      url: `${BASE_URL}/blog/${post.slugs[0]}`,
      lastModified: post.data.date ? new Date(post.data.date) : new Date(),
    }))

  const cliDocEntries: MetadataRoute.Sitemap = cliSource
    .getPages()
    .map((page) => ({
      url: `${BASE_URL}/docs/cli/${page.slugs.join("/")}`,
    }))

  return [
    ...staticRoutes,
    ...blogEntries,
    ...cliDocEntries,
  ]
}
