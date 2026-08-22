import Link from "next/link"
import { SiteHeader } from "@/components/site-header"

export default function NotFound() {
  return (
    <div className="flex min-h-svh flex-col">
      <SiteHeader />
      <main className="flex flex-1 flex-col items-center justify-center px-4">
        <h1 className="text-4xl font-bold text-neutral-900 dark:text-neutral-100">
          Page not found
        </h1>
        <p className="mt-4 text-neutral-600 dark:text-neutral-400">
          The page you&apos;re looking for doesn&apos;t exist.
        </p>
        <Link
          href="/"
          className="mt-6 text-sm font-medium text-blue-600 hover:text-blue-800 dark:text-blue-400 dark:hover:text-blue-300"
        >
          Back to home
        </Link>
      </main>
    </div>
  )
}
