const BLOG_DATE_FORMATTERS = {
  long: new Intl.DateTimeFormat("en-US", {
    year: "numeric",
    month: "long",
    day: "numeric",
    timeZone: "UTC",
  }),
  short: new Intl.DateTimeFormat("en-US", {
    year: "numeric",
    month: "short",
    day: "numeric",
    timeZone: "UTC",
  }),
}

export function formatBlogDate(
  isoDate: string,
  month: keyof typeof BLOG_DATE_FORMATTERS = "long"
): string {
  return BLOG_DATE_FORMATTERS[month].format(new Date(isoDate))
}
