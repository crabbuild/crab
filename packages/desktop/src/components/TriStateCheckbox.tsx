import { Checkbox } from "@/components/ui/checkbox";

export type TriStateValue = "empty" | "partial" | "full";

interface Props {
  state: TriStateValue;
  onActivate: () => void;
  label: string;
  disabled?: boolean;
  className?: string;
}

export function TriStateCheckbox({
  state,
  onActivate,
  label,
  disabled,
  className,
}: Props) {
  return (
    <Checkbox
      checked={state === "partial" ? "indeterminate" : state === "full"}
      disabled={disabled}
      aria-label={label}
      className={className}
      onClick={(event) => event.stopPropagation()}
      onCheckedChange={onActivate}
    />
  );
}
