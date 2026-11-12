import { Button } from "@/components/ui/button";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { Check, ChevronsUpDown, Loader2 } from "lucide-react";

export function Combobox({
  value,
  onValueChange,
  options,
  open,
  onOpenChange,
  loading,
  placeholder,
  searchPlaceholder,
  emptyMessage,
}: {
  value: string;
  onValueChange: (value: string) => void;
  options: {
    value: string;
    label: string;
    detail?: string;
    disabled?: boolean;
  }[];
  open: boolean;
  onOpenChange: (open: boolean) => void;
  loading?: boolean;
  placeholder: string;
  searchPlaceholder: string;
  emptyMessage: string;
}) {
  return (
    <Popover open={open} onOpenChange={onOpenChange}>
      <PopoverTrigger asChild>
        <Button
          role="combobox"
          aria-expanded={open}
          aria-label={placeholder}
          className="mb-1.5 w-full justify-between px-2 text-xs"
        >
          <span
            className={value ? "truncate" : "truncate text-muted-foreground"}
          >
            {value || placeholder}
          </span>
          <ChevronsUpDown
            size={12}
            className="shrink-0 text-muted-foreground"
          />
        </Button>
      </PopoverTrigger>
      <PopoverContent
        align="start"
        className="w-[var(--radix-popover-trigger-width)] min-w-64 p-0"
      >
        <Command defaultValue={":" + value}>
          <div className="border-b px-2">
            <CommandInput
              placeholder={searchPlaceholder}
              aria-label={searchPlaceholder}
              className="h-8 text-xs"
            />
          </div>
          <CommandList className="max-h-52">
            {loading ? (
              <div className="flex items-center gap-2 px-3 py-2 text-xs text-muted-foreground">
                <Loader2 size={12} className="animate-spin" />
                Loading…
              </div>
            ) : (
              <>
                <CommandEmpty className="py-3 text-xs">
                  {emptyMessage}
                </CommandEmpty>
                <CommandGroup>
                  {options.map((option) => (
                    <CommandItem
                      key={option.value}
                      value={":" + option.value}
                      keywords={[option.label, option.detail ?? ""]}
                      disabled={option.disabled}
                      onSelect={() => {
                        onValueChange(option.value);
                        onOpenChange(false);
                      }}
                      className="text-xs"
                    >
                      <div className="min-w-0 flex-1">
                        <div className="truncate">{option.label}</div>
                        {option.detail && (
                          <div className="truncate font-mono text-[10px] text-muted-foreground">
                            {option.detail}
                          </div>
                        )}
                      </div>
                      {option.value === value && <Check size={12} />}
                    </CommandItem>
                  ))}
                </CommandGroup>
              </>
            )}
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}
