import { MarketingLoading } from "@/components/marketing/loading-fallback";

// Root loading fallback — applies to the landing page (`/`) and any
// route segment that does not declare its own `loading.tsx`. Marketing
// routes share the same fade-in placeholder. Docs routes (`/docs/*`)
// override this via Fumadocs' own loading UI.
export default function Loading() {
  return <MarketingLoading />;
}
