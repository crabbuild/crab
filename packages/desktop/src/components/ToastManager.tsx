import { Toaster } from "@/components/ui/sonner";

export function ToastManager() {
  return <Toaster position="bottom-right" closeButton visibleToasts={5} />;
}
