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
      className="not-prose mt-12 overflow-hidden rounded-2xl border-2 border-[#163052] bg-[#f4f7f9] text-[#142033] shadow-[0_18px_45px_rgba(20,32,51,0.12)]"
    >
      <header className="flex flex-col gap-3 border-b border-[#b9c7d8] px-5 py-5 sm:flex-row sm:items-center sm:justify-between sm:px-7">
        <div>
          <p className="m-0 font-mono text-[10px] font-black tracking-[0.18em] text-[#2f6fce]">
            KNOWLEDGE PROOF
          </p>
          <h2
            id={`knowledge-check-${slug}`}
            className="m-0 mt-1 text-xl font-black tracking-[-0.03em]"
          >
            Check the decision, not your memory.
          </h2>
        </div>
        <span className="inline-flex w-fit items-center gap-1.5 rounded-full border border-[#b9c7d8] bg-white px-3 py-1.5 font-mono text-[9px] font-black text-[#52637a]">
          {passed ? (
            <ShieldCheck className="size-4 text-[#3d9b72]" aria-hidden="true" />
          ) : (
            <span
              className="size-2 rounded-full bg-[#e9784a]"
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
                  "flex min-h-12 items-start gap-3 rounded-xl border bg-white px-4 py-3 text-left text-sm leading-6 transition-colors outline-none focus-visible:ring-2 focus-visible:ring-[#2f6fce] focus-visible:ring-offset-2 disabled:cursor-default",
                  isCorrect && "border-[#3d9b72] bg-[#e9f6ef]",
                  isWrong && "border-[#e9784a] bg-[#fff0eb]",
                  !checked && isSelected && "border-[#2f6fce] bg-[#eaf1fc]",
                  !checked &&
                    !isSelected &&
                    "border-[#b9c7d8] hover:border-[#2f6fce]"
                )}
              >
                <span
                  className={cn(
                    "mt-0.5 flex size-5 shrink-0 items-center justify-center rounded-full border font-mono text-[9px] font-black",
                    isCorrect && "border-[#3d9b72] bg-[#3d9b72] text-white",
                    isWrong && "border-[#e9784a] bg-[#e9784a] text-white",
                    !isCorrect &&
                      !isWrong &&
                      isSelected &&
                      "border-[#2f6fce] text-[#2f6fce]",
                    !isCorrect &&
                      !isWrong &&
                      !isSelected &&
                      "border-[#9cabbc] text-[#607188]"
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
              className="min-h-11 rounded-lg bg-[#163052] px-4 py-2 text-sm font-bold text-white transition-colors outline-none hover:bg-[#23466f] focus-visible:ring-2 focus-visible:ring-[#2f6fce] focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-45"
            >
              Check my answer
            </button>
          ) : !passed ? (
            <button
              type="button"
              onClick={retry}
              className="inline-flex min-h-11 items-center gap-2 rounded-lg border border-[#163052] bg-white px-4 py-2 text-sm font-bold outline-none hover:bg-[#eaf1fc] focus-visible:ring-2 focus-visible:ring-[#2f6fce] focus-visible:ring-offset-2"
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
                ? "border-[#3d9b72] bg-[#e9f6ef]"
                : "border-[#e9784a] bg-[#fff0eb]"
            )}
          >
            <p className="m-0 font-bold">
              {passed
                ? "Correct — keep this boundary."
                : "Not yet — use the system boundary."}
            </p>
            <p className="m-0 mt-1 text-[#52637a]">{check.explanation}</p>
          </div>
        )}
      </div>
    </section>
  )
}
