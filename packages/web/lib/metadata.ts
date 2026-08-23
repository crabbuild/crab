import type { Metadata } from "next"

export const SITE_URL = "https://crab.build"
export const SITE_NAME = "Crab"
export const SITE_DESCRIPTION =
  "Git remote helper for cloud object storage. No servers, no LFS endpoints."
export const CRAB_LOGO_PATH = "/crab.optimized.svg"

interface PageMetadataOptions {
  title: string
  description: string
  path: string
  absoluteTitle?: boolean
  article?: {
    publishedTime: string
    authors: string[]
    tags?: string[]
  }
}

export function createPageMetadata({
  title,
  description,
  path,
  absoluteTitle = false,
  article,
}: PageMetadataOptions): Metadata {
  const sharedOpenGraph = {
    title,
    description,
    url: path,
    siteName: SITE_NAME,
    locale: "en_US",
    images: [
      {
        url: "/opengraph-image",
        width: 1200,
        height: 630,
        alt: "Crab — serverless Git for large files in cloud object storage",
      },
    ],
  }

  return {
    title: absoluteTitle ? { absolute: title } : title,
    description,
    alternates: { canonical: path },
    openGraph: article
      ? {
          ...sharedOpenGraph,
          type: "article",
          publishedTime: article.publishedTime,
          authors: article.authors,
          tags: article.tags,
        }
      : { ...sharedOpenGraph, type: "website" },
    twitter: {
      card: "summary_large_image",
      title,
      description,
      images: [
        {
          url: "/twitter-image",
          alt: "Crab — serverless Git for large files in cloud object storage",
        },
      ],
    },
  }
}
