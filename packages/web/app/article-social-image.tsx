import { readFile } from "node:fs/promises"
import { join } from "node:path"

import { ImageResponse } from "next/og"

import { getBlogPost } from "@/lib/blog-posts"
import { getLibraryGuide } from "@/lib/library-guides"

const logoDataUrl = readFile(
  join(process.cwd(), "public", "crab.optimized.svg"),
  "base64"
).then((logo) => `data:image/svg+xml;base64,${logo}`)

export const ARTICLE_SOCIAL_IMAGE_SIZE = { width: 1200, height: 630 }

export function createBlogSocialImage(slug: string) {
  const post = getBlogPost(slug)

  return createArticleSocialImage(
    post?.title ?? "Crab Blog",
    slug === "git-for-large-files-at-any-scale"
      ? "LAUNCH STORY"
      : (post?.category.toUpperCase() ?? "CRAB BLOG")
  )
}

export function createLibrarySocialImage(slug: string) {
  const guide = getLibraryGuide(slug)

  return createArticleSocialImage(
    guide?.title ?? "Crab Library",
    guide?.category.toUpperCase() ?? "CRAB LIBRARY"
  )
}

async function createArticleSocialImage(title: string, label: string) {
  const logo = await logoDataUrl
  const titleSize = title.length > 52 ? 62 : title.length > 36 ? 72 : 84

  return new ImageResponse(
    <div
      style={{
        background: "#07111d",
        color: "#f8fafc",
        display: "flex",
        height: "100%",
        overflow: "hidden",
        position: "relative",
        width: "100%",
      }}
    >
      <div
        style={{
          backgroundImage:
            "linear-gradient(rgba(148,163,184,.07) 1px, transparent 1px), linear-gradient(90deg, rgba(148,163,184,.07) 1px, transparent 1px)",
          backgroundSize: "44px 44px",
          display: "flex",
          inset: 0,
          position: "absolute",
        }}
      />
      <div
        style={{
          background: "rgba(34,211,238,.12)",
          borderRadius: 999,
          display: "flex",
          filter: "blur(80px)",
          height: 420,
          position: "absolute",
          right: -80,
          top: -150,
          width: 620,
        }}
      />

      <div
        style={{
          display: "flex",
          flexDirection: "column",
          padding: "64px 72px",
          position: "relative",
          width: "100%",
        }}
      >
        <div
          style={{
            alignItems: "center",
            display: "flex",
            justifyContent: "space-between",
          }}
        >
          <div style={{ alignItems: "center", display: "flex", gap: 18 }}>
            <div
              style={{
                alignItems: "center",
                background: "#ffffff",
                borderRadius: 14,
                display: "flex",
                height: 64,
                justifyContent: "center",
                width: 64,
              }}
            >
              {/* ImageResponse requires a plain image element for embedded data URLs. */}
              {/* eslint-disable-next-line @next/next/no-img-element */}
              <img alt="" height="48" src={logo} width="48" />
            </div>
            <div style={{ fontSize: 28, fontWeight: 650 }}>Crab</div>
          </div>
          <div
            style={{
              color: "#67e8f9",
              fontFamily: "monospace",
              fontSize: 18,
              fontWeight: 700,
              letterSpacing: 4,
            }}
          >
            {label}
          </div>
        </div>

        <div
          style={{
            display: "flex",
            flex: 1,
            flexDirection: "column",
            justifyContent: "center",
            maxWidth: 980,
          }}
        >
          <div
            style={{
              fontSize: titleSize,
              fontWeight: 700,
              letterSpacing: -4,
              lineHeight: 0.96,
            }}
          >
            {title}
          </div>
        </div>

        <div
          style={{
            alignItems: "center",
            borderTop: "1px solid rgba(148,163,184,.22)",
            display: "flex",
            justifyContent: "space-between",
            paddingTop: 24,
          }}
        >
          <div style={{ color: "#94a3b8", fontSize: 21 }}>
            One history · two data paths · your object store
          </div>
          <div style={{ color: "#38bdf8", fontSize: 21 }}>crab.build</div>
        </div>
      </div>
    </div>,
    ARTICLE_SOCIAL_IMAGE_SIZE
  )
}
