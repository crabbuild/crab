import { toast } from "sonner";

export interface Toast {
  id: string;
  kind: "info" | "success" | "error" | "warning";
  title: string;
  body?: string;
  duration?: number;
  action?: { label: string; onClick: () => void };
}

let nextId = 0;
export function publishToast({
  kind,
  title,
  body,
  duration = 5000,
  action,
}: Omit<Toast, "id">): string {
  const id = `toast-${++nextId}`;
  toast[kind](title, {
    id,
    description: body, duration: duration === 0 ? Infinity : duration,
    action: action && {
      label: action.label,
      onClick: (event) => {
        event.preventDefault();
        action.onClick();
  },
    },
  });
  return id;
}

export function dismissToast(id: string): void {
  toast.dismiss(id);
}
export function usePublishToast() {
  return publishToast;
}
