"use client"

import { ShieldCheck } from "lucide-react"
import { useEffect, useState } from "react"

const STORAGE_KEY = "crab-library-completed"

export function LibraryProgress({ total }: { total: number }) {
  const [completed, setCompleted] = useState(0)

  useEffect(() => {
    function readProgress() {
      try {
        const saved = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "[]")
        setCompleted(Array.isArray(saved) ? new Set(saved).size : 0)
      } catch {
        setCompleted(0)
      }
    }

    readProgress()
    window.addEventListener("storage", readProgress)
    window.addEventListener("crab-library-progress", readProgress)

    return () => {
      window.removeEventListener("storage", readProgress)
      window.removeEventListener("crab-library-progress", readProgress)
    }
  }, [])

  const safeCompleted = Math.min(completed, total)
  const percent = total === 0 ? 0 : Math.round((safeCompleted / total) * 100)

  return (
    <div className="mt-7 max-w-2xl rounded-xl border border-[#b9c7d8] bg-white p-4">
      <div className="flex items-center justify-between gap-4 text-xs font-bold">
        <span className="inline-flex items-center gap-2">
          <ShieldCheck className="size-4 text-[#3d9b72]" aria-hidden="true" />
          Knowledge proofs
        </span>
        <span aria-live="polite">
          {safeCompleted} / {total} verified
        </span>
      </div>
      <div
        className="mt-3 h-2 overflow-hidden rounded-full bg-[#dbe5f2]"
        role="progressbar"
        aria-label="Library knowledge checks completed"
        aria-valuemin={0}
        aria-valuemax={total}
        aria-valuenow={safeCompleted}
      >
        <div
          className="h-full rounded-full bg-[#3d9b72] transition-[width] duration-300"
          style={{ width: `${percent}%` }}
        />
      </div>
    </div>
  )
}
