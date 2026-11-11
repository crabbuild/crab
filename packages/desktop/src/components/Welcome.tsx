import { useState } from "react";
import clsx from "clsx";
import {
  FolderOpen,
  Layers,
  GitCommitHorizontal,
  HardDrive,
} from "lucide-react";
import { useSettings } from "../settings";

interface Card {
  title: string;
  description: string;
  icon: React.ComponentType<{ className?: string }>;
}

const CARDS: Card[] = [
  {
    title: "Open or Clone",
    description:
      "Open a local repository from your filesystem, or clone directly from a Crab URL. " +
      "Your repos appear in the sidebar for quick switching.",
    icon: FolderOpen,
  },
  {
    title: "Changes & Staging",
    description:
      "The Changes view shows every modified file with a full patch diff. " +
      "Stage individual hunks or lines — no need to stage entire files at once.",
    icon: Layers,
  },
  {
    title: "Commit & Push",
    description:
      "Write your commit message in the composer at the bottom of the Changes view, " +
      "then push to your remote in one click. Amend, sign-off, and co-authors are built in.",
    icon: GitCommitHorizontal,
  },
  {
    title: "Pointer Files",
    description:
      "Crab stores large files as lightweight pointer blobs. Hydrate them on demand " +
      "or dehydrate to reclaim disk space — all without leaving the app.",
    icon: HardDrive,
  },
];

export function Welcome() {
  const { settings, update } = useSettings();
  const [step, setStep] = useState(0);

  // Don't render if the user has already dismissed the tour.
  if (settings.welcomeSeen) return null;

  const dismiss = () => update("welcomeSeen", true);
  const isLast = step === CARDS.length - 1;

  return (
    <Backdrop>
      <div className="relative flex w-full max-w-md flex-col items-center rounded-xl border border-border bg-card p-8 shadow-2xl">
        <CardContent card={CARDS[step]} />

        {/* Dot indicators */}
        <div className="mt-6 flex gap-2">
          {CARDS.map((_, i) => (
            <span
              key={i}
              className={clsx(
                "h-2 w-2 rounded-full transition-colors",
                i === step ? "bg-primary" : "bg-muted-foreground/40",
              )}
            />
          ))}
        </div>

        {/* Navigation buttons */}
        <div className="mt-6 flex w-full items-center justify-between">
          <button
            type="button"
            onClick={dismiss}
            className="rounded px-3 py-1.5 text-sm text-muted-foreground hover:text-foreground transition-colors"
          >
            Skip
          </button>

          {isLast ? (
            <button
              type="button"
              onClick={dismiss}
              className="rounded-md bg-primary px-4 py-1.5 text-sm font-medium text-primary-foreground hover:bg-primary/90 transition-colors"
            >
              Get Started
            </button>
          ) : (
            <button
              type="button"
              onClick={() => setStep((s) => s + 1)}
              className="rounded-md bg-primary px-4 py-1.5 text-sm font-medium text-primary-foreground hover:bg-primary/90 transition-colors"
            >
              Next
            </button>
          )}
        </div>
      </div>
    </Backdrop>
  );
}

function Backdrop({ children }: { children: React.ReactNode }) {
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
      {children}
    </div>
  );
}

function CardContent({ card }: { card: Card }) {
  const Icon = card.icon;
  return (
    <div className="flex flex-col items-center text-center">
      <div className="mb-4 flex h-16 w-16 items-center justify-center rounded-full bg-primary/10">
        <Icon className="h-8 w-8 text-primary" />
      </div>
      <h2 className="text-lg font-semibold text-foreground">{card.title}</h2>
      <p className="mt-2 text-sm leading-relaxed text-muted-foreground">{card.description}</p>
    </div>
  );
}
