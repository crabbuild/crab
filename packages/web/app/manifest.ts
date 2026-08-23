import type { MetadataRoute } from "next"

import { CRAB_LOGO_PATH, SITE_DESCRIPTION, SITE_NAME } from "@/lib/metadata"

export default function manifest(): MetadataRoute.Manifest {
  return {
    name: `${SITE_NAME} — Serverless Git Remote`,
    short_name: SITE_NAME,
    description: SITE_DESCRIPTION,
    start_url: "/",
    display: "standalone",
    background_color: "#ffffff",
    theme_color: "#000000",
    icons: [
      {
        src: CRAB_LOGO_PATH,
        sizes: "any",
        type: "image/svg+xml",
      },
      {
        src: "/apple-icon.png",
        sizes: "180x180",
        type: "image/png",
      },
    ],
  }
}
