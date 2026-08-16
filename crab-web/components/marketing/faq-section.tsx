"use client"

import { ChevronDown } from "lucide-react"

import { Badge } from "@/components/ui/badge"
import { Reveal } from "@/components/marketing/reveal"

// ─── FAQ data ────────────────────────────────────────────────────────────────

interface FAQItem {
  question: string
  answer: string
}

const FAQ_ITEMS: FAQItem[] = [
  {
    question: "What is Crab?",
    answer:
      "Crab is a serverless Git remote storage solution powered by the Xet protocol. It acts as a Git remote helper and filter driver, enabling you to version any file — regardless of size — directly into your own cloud storage bucket (S3, GCS, or Azure Blob).",
  },
  {
    question: "How is Crab different from Git LFS?",
    answer:
      "Git LFS stores files as whole objects on a dedicated server you have to run and scale. Crab uses content-defined chunking (CDC) to split files at natural byte boundaries and deduplicates at the chunk level. There's no server — data goes straight to your cloud bucket. Identical chunks are uploaded exactly once, even across different files and repos.",
  },
  {
    question: "What cloud providers are supported?",
    answer:
      "Crab supports Amazon S3, Google Cloud Storage (GCS), and Azure Blob Storage. Your data stays in your bucket under your control. We also support S3-compatible endpoints like MinIO and Cloudflare R2.",
  },
  {
    question: "How does chunk-level deduplication work?",
    answer:
      "Crab uses Gearhash content-defined chunking (CDC) to split files at natural content boundaries. Each chunk is hashed and checked against a 3-tier deduplication index (session, shard, and database). Chunks that already exist on the remote are skipped — only truly new data is uploaded. This typically saves 50–90% on storage for versioned binary files.",
  },
  {
    question: "Can I use Crab with my existing Git repos?",
    answer:
      "Absolutely. Crab works with standard Git commands — git clone, add, commit, push. You add a Crab remote to your existing repo and push. If you're migrating from Git LFS, Crab can detect LFS pointers and store those files alongside xorbs without re-uploading.",
  },
  {
    question: "How do I get started?",
    answer:
      "Install Crab with a single command: brew install crabbuild/tap/crab on macOS or Linux with Homebrew, curl -fsSL https://crab.build/install.sh | bash on Linux, or irm https://crab.build/install.ps1 | iex on Windows. Then run crab init in your repo to configure a cloud remote.",
  },
]

// ─── Component ───────────────────────────────────────────────────────────────

export function FAQSection() {
  return (
    <section
      className="w-full px-6 py-section"
      aria-labelledby="faq-heading"
    >
      <div className="mx-auto max-w-3xl">
        {/* Section heading */}
        <Reveal>
          <div className="text-center">
            <Badge variant="outline" className="mb-4">
              FAQ
            </Badge>
            <h2
              id="faq-heading"
              className="text-3xl font-bold tracking-tight text-foreground md:text-4xl"
            >
              Frequently asked questions
            </h2>
            <p className="mt-4 text-lg text-muted-foreground">
              Everything you need to know about Crab
            </p>
          </div>
        </Reveal>

        {/* FAQ accordion list */}
        <div className="mt-12 divide-y divide-border" role="list">
          {FAQ_ITEMS.map((item) => (
            <Reveal key={item.question}>
              <details
                className="faq-item group"
                role="listitem"
              >
                <summary className="flex w-full cursor-pointer items-center justify-between gap-4 py-4 text-left font-semibold text-foreground">
                  <span>{item.question}</span>
                  <ChevronDown
                    size={20}
                    strokeWidth={2}
                    className="faq-chevron shrink-0 text-muted-foreground"
                    aria-hidden="true"
                  />
                </summary>
                <div className="faq-content text-muted-foreground">
                  <p>{item.answer}</p>
                </div>
              </details>
            </Reveal>
          ))}
        </div>
      </div>
    </section>
  )
}
