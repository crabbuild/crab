import "@github/relative-time-element";
import type { RelativeTimeElement } from "@github/relative-time-element";
import type {
  DetailedHTMLProps,
  HTMLAttributes as TimeAttributes,
} from "react";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";

declare module "react" {
  namespace JSX {
    interface IntrinsicElements {
      "relative-time": DetailedHTMLProps<
        TimeAttributes<RelativeTimeElement>,
        RelativeTimeElement
      > & {
        datetime: string;
        precision: "minute";
        "no-title": string;
      };
    }
  }
}

export function timestamp(value: number) {
  return new Date(value).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
}

export function RelativeTime({ value }: { value: number | null }) {
  if (value === null) return <span>at an unknown time</span>;
  const absolute = timestamp(value);
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <relative-time
          datetime={new Date(value).toISOString()}
          precision="minute"
          no-title=""
          tabIndex={0}
        />
      </TooltipTrigger>
      <TooltipContent>{absolute}</TooltipContent>
    </Tooltip>
  );
}
