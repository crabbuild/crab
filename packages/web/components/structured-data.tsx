import {
  serializeStructuredData,
  type StructuredDataValue,
} from "@/lib/structured-data"

export function StructuredData({ data }: { data: StructuredDataValue }) {
  return (
    <script
      type="application/ld+json"
      dangerouslySetInnerHTML={{ __html: serializeStructuredData(data) }}
    />
  )
}
