import { createSocialImage } from "./social-image"

export const alt =
  "Crab — serverless Git for large files in cloud object storage"
export const size = { width: 1200, height: 630 }
export const contentType = "image/png"

export default function TwitterImage() {
  return createSocialImage()
}
