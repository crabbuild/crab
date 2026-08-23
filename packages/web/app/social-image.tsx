import { readFile } from "node:fs/promises"
import { join } from "node:path"

import { ImageResponse } from "next/og"

const SOCIAL_IMAGE_SIZE = { width: 1200, height: 630 }

const logoDataUrl = readFile(
  join(process.cwd(), "public", "crab.optimized.svg"),
  "base64"
).then((logo) => `data:image/svg+xml;base64,${logo}`)

export async function createSocialImage() {
  const logo = await logoDataUrl

  return new ImageResponse(
    <div
      style={{
        alignItems: "center",
        background: "#09090b",
        color: "#fafafa",
        display: "flex",
        height: "100%",
        justifyContent: "center",
        width: "100%",
      }}
    >
      <div
        style={{
          alignItems: "center",
          display: "flex",
          gap: 56,
          padding: "72px 88px",
        }}
      >
        <div
          style={{
            alignItems: "center",
            background: "#ffffff",
            borderRadius: 48,
            display: "flex",
            height: 240,
            justifyContent: "center",
            width: 240,
          }}
        >
          {/* ImageResponse requires a plain image element for embedded data URLs. */}
          {/* eslint-disable-next-line @next/next/no-img-element */}
          <img alt="" height="190" src={logo} width="190" />
        </div>
        <div
          style={{ display: "flex", flexDirection: "column", maxWidth: 650 }}
        >
          <div style={{ fontSize: 76, fontWeight: 700, letterSpacing: -3 }}>
            Crab
          </div>
          <div style={{ color: "#d4d4d8", fontSize: 38, lineHeight: 1.25 }}>
            Serverless Git for large files in your cloud storage
          </div>
        </div>
      </div>
    </div>,
    SOCIAL_IMAGE_SIZE
  )
}
