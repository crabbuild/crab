"use client"

import { Dialog } from "@base-ui/react/dialog"
import { Maximize2, X } from "lucide-react"
import Image from "next/image"

const DIAGRAM_ALT =
  "One Git commit connects code and a compact Crab pointer in Git history to immutable large-file data in an object store, producing one visible repository state"

export function CurrentSubjectDiagram() {
  return (
    <aside className="self-start rounded-2xl border border-slate-800 bg-[#07111f] p-1.5 shadow-[0_22px_70px_rgba(2,6,23,0.2)] lg:self-center">
      <Dialog.Root>
        <Dialog.Trigger
          type="button"
          className="group relative block w-full cursor-zoom-in overflow-hidden rounded-xl text-left outline-none focus-visible:ring-2 focus-visible:ring-cyan-400 focus-visible:ring-offset-2 focus-visible:ring-offset-background"
        >
          <Image
            src="/diagram/blog-cover/current-subject.svg"
            width={560}
            height={400}
            alt={DIAGRAM_ALT}
            className="h-auto w-full transition-transform duration-300 ease-out group-hover:scale-[1.01] motion-reduce:transition-none"
            unoptimized
            priority
          />
          <span className="pointer-events-none absolute right-3 bottom-3 inline-flex items-center gap-1.5 rounded-full border border-cyan-300/30 bg-slate-950/85 px-2.5 py-1.5 font-mono text-[9px] font-bold tracking-[0.12em] text-cyan-100 opacity-0 shadow-lg backdrop-blur-sm transition-opacity group-hover:opacity-100 group-focus-visible:opacity-100 motion-reduce:transition-none">
            <Maximize2 className="size-3" aria-hidden="true" />
            ZOOM
          </span>
        </Dialog.Trigger>

        <Dialog.Portal>
          <Dialog.Backdrop className="fixed inset-0 z-50 bg-slate-950/85 backdrop-blur-sm transition-opacity duration-200 motion-reduce:transition-none data-open:opacity-100 data-closed:opacity-0" />
          <Dialog.Viewport className="fixed inset-0 z-50 grid place-items-center overflow-y-auto p-3 sm:p-6">
            <Dialog.Popup className="w-full max-w-6xl overflow-hidden rounded-2xl border border-slate-700 bg-[#07111f] shadow-2xl transition-[opacity,transform] duration-200 outline-none motion-reduce:transition-none data-open:scale-100 data-open:opacity-100 data-closed:scale-[0.98] data-closed:opacity-0">
              <div className="flex min-h-14 items-center justify-between gap-4 border-b border-slate-800 px-4 sm:px-5">
                <div className="min-w-0 py-3">
                  <Dialog.Title className="truncate text-sm font-bold text-slate-100">
                    One history. Two data paths.
                  </Dialog.Title>
                  <Dialog.Description className="truncate text-xs text-slate-400">
                    Git names the state; your bucket carries the weight.
                  </Dialog.Description>
                </div>
                <Dialog.Close
                  type="button"
                  className="grid size-10 shrink-0 place-items-center rounded-full border border-slate-700 text-slate-300 transition-colors hover:border-cyan-400/60 hover:bg-slate-800 hover:text-white focus-visible:ring-2 focus-visible:ring-cyan-400 focus-visible:outline-none"
                  aria-label="Close expanded diagram"
                >
                  <X className="size-4" aria-hidden="true" />
                </Dialog.Close>
              </div>
              <Image
                src="/diagram/blog-cover/current-subject.svg"
                width={1120}
                height={800}
                alt={DIAGRAM_ALT}
                className="h-auto max-h-[calc(100dvh-7rem)] w-full object-contain"
                unoptimized
              />
            </Dialog.Popup>
          </Dialog.Viewport>
        </Dialog.Portal>
      </Dialog.Root>
    </aside>
  )
}
