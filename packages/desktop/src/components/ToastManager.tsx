import { useEffect, useRef } from "react";
import { useToasts, dismissToast, Toast } from "../lib/toasts";
import {
  CheckCircle2,
  XCircle,
  Info,
  AlertTriangle,
  X,
} from "lucide-react";

// --- Icon + color mapping by kind ---

const kindConfig: Record<
  Toast["kind"],
  { icon: typeof Info; accent: string; bg: string }
> = {
  success: {
    icon: CheckCircle2,
    accent: "text-success",
    bg: "border-success/40",
  },
  error: {
    icon: XCircle,
    accent: "text-destructive",
    bg: "border-destructive/40",
  },
  info: {
    icon: Info,
    accent: "text-info",
    bg: "border-info/40",
  },
  warning: {
    icon: AlertTriangle,
    accent: "text-warning",
    bg: "border-warning/40",
  },
};

// --- Individual toast card ---

function ToastCard({ toast }: { toast: Toast }) {
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const { icon: Icon, accent, bg } = kindConfig[toast.kind];

  useEffect(() => {
    const duration = toast.duration ?? 5000;
    if (duration > 0) {
      timerRef.current = setTimeout(() => dismissToast(toast.id), duration);
    }
    return () => {
      if (timerRef.current) clearTimeout(timerRef.current);
    };
  }, [toast.id, toast.duration]);

  return (
    <div
      className={`animate-slide-in-right flex w-80 items-start gap-3 rounded-lg border ${bg} bg-card p-3 shadow-lg`}
      role="alert"
      aria-live="assertive"
    >
      <Icon className={`mt-0.5 h-5 w-5 shrink-0 ${accent}`} aria-hidden />
      <div className="min-w-0 flex-1">
        <p className="text-sm font-semibold text-foreground">{toast.title}</p>
        {toast.body && (
          <p className="mt-0.5 text-xs text-muted-foreground">{toast.body}</p>
        )}
        {toast.action && (
          <button
            type="button"
            className="mt-1.5 text-xs font-medium text-info hover:text-info"
            onClick={toast.action.onClick}
          >
            {toast.action.label}
          </button>
        )}
      </div>
      <button
        type="button"
        className="shrink-0 rounded p-0.5 text-muted-foreground hover:text-foreground focus-visible:outline focus-visible:outline-2 focus-visible:outline-blue-500"
        onClick={() => dismissToast(toast.id)}
        aria-label="Dismiss notification"
      >
        <X className="h-4 w-4" />
      </button>
    </div>
  );
}

// --- ToastManager: mounts once at app root ---

export function ToastManager() {
  const toasts = useToasts();

  if (toasts.length === 0) return null;

  return (
    <div
      className="pointer-events-none fixed bottom-4 right-4 z-50 flex flex-col-reverse items-end gap-2"
      aria-label="Notifications"
    >
      {toasts.map((t) => (
        <div key={t.id} className="pointer-events-auto">
          <ToastCard toast={t} />
        </div>
      ))}
    </div>
  );
}
