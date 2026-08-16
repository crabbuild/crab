// Override the root marketing `loading.tsx` for documentation routes.
// Fumadocs' `DocsLayout` provides its own loading affordances (sidebar
// remains rendered while content streams), so we render nothing here
// rather than showing the marketing fade-in placeholder.
export default function Loading() {
  return null;
}
