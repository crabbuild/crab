import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/utils";
import type { AriaAttributes, ComponentProps, ReactNode, Ref } from "react";

type Option = {
  value: string;
  label: ReactNode;
  disabled?: boolean;
  group?: string;
};

type FieldSelectProps = Pick<
  ComponentProps<typeof Select>,
  | "value"
  | "defaultValue"
  | "onValueChange"
  | "onOpenChange"
  | "disabled"
  | "name"
  | "required"
> &
  AriaAttributes & {
    options: Readonly<Record<string, string>> | Option[];
    id?: string;
    label?: string;
    className?: string;
    ref?: Ref<HTMLButtonElement>;
  };

export function FieldSelect({
  label,
  options,
  id,
  className,
  ref,
  disabled,
  name,
  required,
  value,
  defaultValue,
  onValueChange,
  onOpenChange,
  ...accessibility
}: FieldSelectProps) {
  const items: Option[] = Array.isArray(options)
    ? options
    : Object.entries(options).map(([value, label]) => ({ value, label }));
  const groups = new Map<string | undefined, Option[]>();
  for (const item of items) {
    const group = groups.get(item.group) ?? [];
    group.push(item);
    groups.set(item.group, group);
  }
  const placeholder = items.find((item) => item.value === "")?.label;
  return (
    <Select
      value={value}
      defaultValue={defaultValue}
      onValueChange={onValueChange}
      onOpenChange={onOpenChange}
      disabled={disabled}
      name={name}
      required={required}
    >
      <SelectTrigger
        id={id}
        ref={ref}
        className={cn("min-w-0 max-w-full", className)}
        aria-label={label}
        {...accessibility}
      >
        <SelectValue placeholder={placeholder} />
      </SelectTrigger>
      <SelectContent position="popper" align="start">
        {[...groups].map(([group, items]) => (
          <SelectGroup key={group ?? ""}>
            {group && <SelectLabel>{group}</SelectLabel>}
            {items.map((item) => (
              <SelectItem
                key={item.value}
                value={item.value}
                data-value={item.value}
                disabled={item.disabled}
              >
                {item.label}
              </SelectItem>
            ))}
          </SelectGroup>
        ))}
      </SelectContent>
    </Select>
  );
}
