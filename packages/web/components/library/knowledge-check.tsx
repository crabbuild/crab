"use client"

import { Check, RotateCcw, ShieldCheck, X } from "lucide-react"
import { useState } from "react"

import type { KnowledgeCheckData } from "@/lib/library"
import { cn } from "@/lib/utils"

export function KnowledgeCheck({
  slug,
  check,
}: {
  slug: string
  check: KnowledgeCheckData
}) {
  const [selected, setSelected] = useState<number | null>(null)
  const [checked, setChecked] = useState(false)
  const passed = checked && selected === check.answer

  function verify() {
    if (selected === null) return
    setChecked(true)

    if (selected === check.answer) {
      const key = "crab-library-completed"
      let existing: string[] = []

      try {
        const saved = JSON.parse(localStorage.getItem(key) ?? "[]")
        if (Array.isArray(saved)) {
          existing = saved.filter(
            (value): value is string => typeof value === "string"
          )
        }
      } catch {
        // A malformed local value should not block a correct answer.
      }

      localStorage.setItem(
        key,
        JSON.stringify([...new Set([...existing, slug])])
      )
      window.dispatchEvent(new Event("crab-library-progress"))
    }
  }

  function retry() {
    setSelected(null)
    setChecked(false)
  }

  return (
    <section
      aria-labelledby={`knowledge-check-${slug}`}
      className="not-prose mt-12 overflow-hidden rounded-2xl border border-border bg-card text-card-foreground shadow-sm"
    >
      <header className="flex flex-col gap-3 border-b border-border px-5 py-5 sm:flex-row sm:items-center sm:justify-between sm:px-7">
        <div>
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.18em] text-primary">
            KNOWLEDGE PROOF
          </p>
          <h2
            id={`knowledge-check-${slug}`}
            className="m-0 mt-1 text-xl font-black tracking-[-0.03em]"
          >
            Check the decision, not your memory.
          </h2>
        </div>
        <span className="inline-flex w-fit items-center gap-1.5 rounded-full border border-border bg-background px-3 py-1.5 font-mono text-[9px] font-black text-muted-foreground">
          {passed ? (
            <ShieldCheck
              className="size-4 text-emerald-600 dark:text-emerald-400"
              aria-hidden="true"
            />
          ) : (
            <span
              className="size-2 rounded-full bg-orange-500"
              aria-hidden="true"
            />
          )}
          {passed ? "CONCEPT VERIFIED" : "ONE QUESTION"}
        </span>
      </header>

      <div className="p-5 sm:p-7">
        <p className="m-0 max-w-3xl text-lg leading-7 font-bold">
          {check.question}
        </p>

        <div
          className="mt-5 grid gap-2"
          role="radiogroup"
          aria-label={check.question}
        >
          {check.options.map((option, index) => {
            const isSelected = selected === index
            const isCorrect = checked && index === check.answer
            const isWrong = checked && isSelected && index !== check.answer

            return (
              <button
                key={option}
                type="button"
                role="radio"
                aria-checked={isSelected}
                disabled={checked}
                onClick={() => setSelected(index)}
                className={cn(
                  "flex min-h-12 items-start gap-3 rounded-xl border bg-background px-4 py-3 text-left text-sm leading-6 transition-colors outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-card disabled:cursor-default",
                  isCorrect &&
                    "border-emerald-500 bg-emerald-50 dark:border-emerald-400 dark:bg-emerald-950/40",
                  isWrong &&
                    "border-orange-500 bg-orange-50 dark:border-orange-400 dark:bg-orange-950/40",
                  !checked && isSelected && "border-primary bg-primary-muted",
                  !checked &&
                    !isSelected &&
                    "border-border hover:border-primary"
                )}
              >
                <span
                  className={cn(
                    "mt-0.5 flex size-5 shrink-0 items-center justify-center rounded-full border font-mono text-[9px] font-black",
                    isCorrect &&
                      "border-emerald-600 bg-emerald-600 text-white dark:border-emerald-400 dark:bg-emerald-400 dark:text-emerald-950",
                    isWrong &&
                      "border-orange-600 bg-orange-600 text-white dark:border-orange-400 dark:bg-orange-400 dark:text-orange-950",
                    !isCorrect &&
                      !isWrong &&
                      isSelected &&
                      "border-primary text-primary",
                    !isCorrect &&
                      !isWrong &&
                      !isSelected &&
                      "border-muted-foreground/50 text-muted-foreground"
                  )}
                >
                  {isCorrect ? (
                    <Check className="size-3" aria-hidden="true" />
                  ) : isWrong ? (
                    <X className="size-3" aria-hidden="true" />
                  ) : (
                    String.fromCharCode(65 + index)
                  )}
                </span>
                <span>{option}</span>
              </button>
            )
          })}
        </div>

        <div className="mt-5 flex flex-wrap items-center gap-3">
          {!checked ? (
            <button
              type="button"
              disabled={selected === null}
              onClick={verify}
              className="min-h-11 rounded-lg bg-foreground px-4 py-2 text-sm font-bold text-background transition-colors outline-none hover:bg-foreground/90 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-card disabled:cursor-not-allowed disabled:opacity-45"
            >
              Check my answer
            </button>
          ) : !passed ? (
            <button
              type="button"
              onClick={retry}
              className="inline-flex min-h-11 items-center gap-2 rounded-lg border border-border bg-background px-4 py-2 text-sm font-bold outline-none hover:bg-muted focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-card"
            >
              <RotateCcw className="size-4" aria-hidden="true" />
              Try again
            </button>
          ) : null}
        </div>

        {checked && (
          <div
            aria-live="polite"
            className={cn(
              "mt-5 rounded-xl border-l-4 p-4 text-sm leading-6",
              passed
                ? "border-emerald-500 bg-emerald-50 dark:border-emerald-400 dark:bg-emerald-950/40"
                : "border-orange-500 bg-orange-50 dark:border-orange-400 dark:bg-orange-950/40"
            )}
          >
            <p className="m-0 font-bold">
              {passed
                ? "Correct — keep this boundary."
                : "Not yet — use the system boundary."}
            </p>
            <p className="m-0 mt-1 text-muted-foreground">
              {check.explanation}
            </p>
          </div>
        )}
      </div>
    </section>
  )
}
