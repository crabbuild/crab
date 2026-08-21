/**
 * Skip-navigation link rendered as the very first focusable element in the
 * page. Visually hidden until it receives keyboard focus, at which point it
 * appears in the top-left corner and lets keyboard users bypass the header
 * and jump straight into the main page content.
 *
 * Targets the element with `id="main-content"`. Marketing pages set this id
 * on the `<main>` landmark; documentation routes set it on a wrapping
 * `<div>` around the Fumadocs `DocsLayout`.
 */
export function SkipNavLink({
  targetId = "main-content",
  children = "Skip to main content",
}: {
  targetId?: string;
  children?: React.ReactNode;
}) {
  return (
    <a
      href={`#${targetId}`}
      className="sr-only focus:not-sr-only focus:absolute focus:top-2 focus:left-2 focus:z-50 focus:rounded-md focus:bg-primary focus:px-3 focus:py-2 focus:text-sm focus:font-medium focus:text-primary-foreground focus:shadow-lg focus:outline-none focus:ring-2 focus:ring-ring"
    >
      {children}
    </a>
  );
}
