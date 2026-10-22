import {
  createContext,
  useContext,
  useEffect,
  useEffectEvent,
  useState,
} from "react";

export const OverlayVisibility = createContext(true);

type Options = {
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
};

export function useOverlayState({
  open,
  defaultOpen = false,
  onOpenChange,
}: Options) {
  const visible = useContext(OverlayVisibility);
  const [current, setCurrent] = useState(defaultOpen);
  function change(next: boolean) {
    if (open === undefined) setCurrent(next);
    onOpenChange?.(next);
  }
  const dismiss = useEffectEvent(() => change(false));
  useEffect(() => {
    if (!visible) dismiss();
  }, [visible]);
  return { open: visible && (open ?? current), onOpenChange: change };
}
