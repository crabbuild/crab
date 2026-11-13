import { Toaster } from "@/components/ui/sonner";
import { useEffectiveTheme } from "@/hooks/useEffectiveTheme";

export function ToastManager() {
  const theme = useEffectiveTheme();
  return <Toaster theme={theme} position="bottom-right" closeButton visibleToasts={5} />;
}
