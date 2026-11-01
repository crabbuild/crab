import { useEffect, useState } from "react";
import {
  format,
  parseISO,
  setMonth,
  setYear,
  startOfDay,
  startOfMonth,
} from "date-fns";
import { useDayPicker, type MonthCaptionProps } from "react-day-picker";
import { CalendarIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Calendar } from "@/components/ui/calendar";
import { FieldSelect } from "@/components/ui/field-select";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { Separator } from "@/components/ui/separator";
import { cn } from "@/lib/utils";

export function DatePicker({
  id,
  label,
  value,
  onValueChange,
  min = "0001-01-01",
  max = "9999-12-31",
  disabled = false,
}: {
  id: string;
  label: string;
  value: string | null;
  onValueChange: (value: string | null) => void;
  min?: string;
  max?: string;
  disabled?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const selected = value ? parseISO(value) : undefined;
  const first = parseISO(min);
  const last = parseISO(max);
  const today = startOfDay(new Date());
  const initial =
    selected ?? (today < first ? first : today > last ? last : today);
  const display = selected ? format(selected, "MMM d, yyyy") : "Pick a date";
  // The calendar portal is outside its form's disabled fieldset. Close it when
  // a submission starts so pending forms cannot keep an interactive picker.
  useEffect(() => {
    if (disabled) setOpen(false);
  }, [disabled]);
  function choose(date: Date | undefined) {
    // Planning dates are civil dates. UTC conversion can change their day.
    onValueChange(date ? format(date, "yyyy-MM-dd") : null);
    setOpen(false);
  }
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          id={id}
          variant="outline"
          disabled={disabled}
          className="w-full justify-between"
          aria-label={`${label}: ${display}`}
        >
          <span
            className={cn("font-normal", !value && "text-muted-foreground")}
          >
            {display}
          </span>
          <CalendarIcon data-icon="inline-end" />
        </Button>
      </PopoverTrigger>
      <PopoverContent
        align="start"
        className="max-h-(--radix-popover-content-available-height) w-auto overflow-y-auto p-0"
        aria-label={`${label} calendar`}
      >
        <Calendar
          mode="single"
          selected={selected}
          defaultMonth={initial}
          onSelect={choose}
          startMonth={first}
          endMonth={last}
          disabled={disabled ? true : [{ before: first }, { after: last }]}
          components={{ MonthCaption: DateCaption }}
          autoFocus
        />
        <Separator />
        <div className="flex justify-between gap-2 p-2">
          <Button
            size="sm"
            variant="ghost"
            disabled={disabled || !selected}
            onClick={() => choose(undefined)}
          >
            Clear date
          </Button>
          <Button
            size="sm"
            variant="ghost"
            disabled={disabled || today < first || today > last}
            onClick={() => choose(today)}
          >
            Today
          </Button>
        </div>
      </PopoverContent>
    </Popover>
  );
}

function DateCaption({
  calendarMonth,
  displayIndex: _displayIndex,
  children,
  className,
  ...props
}: MonthCaptionProps) {
  const { goToMonth, dayPickerProps } = useDayPicker();
  const current = startOfMonth(calendarMonth.date);
  const year = current.getFullYear();
  // DatePicker supplies both civil-date bounds to its calendar.
  const lower = dayPickerProps.startMonth!;
  const upper = dayPickerProps.endMonth!;
  const months = Object.fromEntries(
    Array.from({ length: 12 }, (_, month) => setMonth(current, month))
      .filter(
        (date) => date >= startOfMonth(lower) && date <= startOfMonth(upper),
      )
      .map((date) => [String(date.getMonth()), format(date, "MMM")]),
  );
  const firstYear = Math.max(lower.getFullYear(), year - 100);
  const lastYear = Math.min(upper.getFullYear(), year + 100);
  const years = Object.fromEntries(
    Array.from({ length: lastYear - firstYear + 1 }, (_, index) => {
      const value = String(firstYear + index);
      return [value, value];
    }),
  );
  return (
    <div className={cn(className, "gap-1")} {...props}>
      <FieldSelect
        label="Choose month"
        value={String(current.getMonth())}
        options={months}
        onValueChange={(month) => goToMonth(setMonth(current, Number(month)))}
      />
      <FieldSelect
        label="Choose year"
        value={String(year)}
        options={years}
        onValueChange={(year) => goToMonth(setYear(current, Number(year)))}
      />
      <span className="sr-only">{children}</span>
    </div>
  );
}
