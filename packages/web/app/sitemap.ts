import type { MetadataRoute } from "next"
import { blogSource } from "@/lib/blog-source"
import { SITE_URL } from "@/lib/metadata"
import { cliSource } from "@/lib/source"

export default function sitemap(): MetadataRoute.Sitemap {
  const staticRoutes: MetadataRoute.Sitemap = [
    { url: `${SITE_URL}/`, changeFrequency: "weekly", priority: 1 },
    { url: `${SITE_URL}/about-us` },
    { url: `${SITE_URL}/auth` },
    { url: `${SITE_URL}/blog`, changeFrequency: "weekly", priority: 0.8 },
    { url: `${SITE_URL}/cache` },
    { url: `${SITE_URL}/changelog`, changeFrequency: "weekly" },
    { url: `${SITE_URL}/cli`, priority: 0.9 },
    { url: `${SITE_URL}/docs`, priority: 0.9 },
    { url: `${SITE_URL}/integrations` },
    { url: `${SITE_URL}/pricing` },
    { url: `${SITE_URL}/privacy` },
    { url: `${SITE_URL}/remote-services` },
    { url: `${SITE_URL}/terms-of-service` },
    { url: `${SITE_URL}/use-cases` },
  ]

  const blogEntries: MetadataRoute.Sitemap = blogSource
    .getPages()
    .map((post) => ({
      url: `${SITE_URL}/blog/${post.slugs[0]}`,
      lastModified: post.data.date ? new Date(post.data.date) : new Date(),
      changeFrequency: "monthly",
      priority: 0.7,
    }))

  const cliDocEntries: MetadataRoute.Sitemap = cliSource
    .getPages()
    .map((page) => ({
      url: `${SITE_URL}/docs/cli/${page.slugs.join("/")}`,
      changeFrequency: "weekly",
      priority: 0.8,
    }))

  return [...staticRoutes, ...blogEntries, ...cliDocEntries]
}
