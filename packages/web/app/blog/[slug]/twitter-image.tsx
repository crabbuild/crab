import {
  ARTICLE_SOCIAL_IMAGE_SIZE,
  createBlogSocialImage,
} from "../../article-social-image"

export const alt = "Crab technical blog article"
export const size = ARTICLE_SOCIAL_IMAGE_SIZE
export const contentType = "image/png"

export default async function TwitterImage({
  params,
}: {
  params: Promise<{ slug: string }>
}) {
  const { slug } = await params
  return createBlogSocialImage(slug)
}
