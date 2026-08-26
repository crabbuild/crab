import {
  ARTICLE_SOCIAL_IMAGE_SIZE,
  createLibrarySocialImage,
} from "../../article-social-image"

export const alt = "Crab technical library guide"
export const size = ARTICLE_SOCIAL_IMAGE_SIZE
export const contentType = "image/png"

export default async function TwitterImage({
  params,
}: {
  params: Promise<{ slug: string }>
}) {
  const { slug } = await params
  return createLibrarySocialImage(slug)
}
