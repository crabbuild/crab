import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useCallback } from "react";
import type { ConditionSpec } from "../../lib/workflow-types";

interface Props {
  condition: ConditionSpec | undefined;
  onChange: (condition: ConditionSpec | undefined) => void;
}

export function ConditionBuilder({ condition, onChange }: Props) {
  const handleKindChange = useCallback(
    (kind: string) => {
      if (kind === "none") {
        onChange(undefined);
      } else {
        onChange({
          kind: kind as ConditionSpec["kind"],
          value: condition?.value ?? "",
        });
      }
    },
    [condition, onChange],
  );

  const handleValueChange = useCallback(
    (value: string) => {
      if (!condition) return;
      onChange({ ...condition, value });
    },
    [condition, onChange],
  );

  return (
    <div className="space-y-2">
      <Select
        value={":" + String(condition?.kind ?? "none")}
        onValueChange={(selectedValue) => {
          const value = selectedValue.slice(1);
          return handleKindChange(value);
        }}
      >
        <SelectTrigger className="w-full text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectGroup>
            <SelectItem value={":" + String("none")}>No condition</SelectItem>
            <SelectItem value={":" + String("env")}>
              Environment variable
            </SelectItem>
            <SelectItem value={":" + String("file_exists")}>
              File exists
            </SelectItem>
            <SelectItem value={":" + String("expr")}>Expression</SelectItem>
          </SelectGroup>
        </SelectContent>
      </Select>

      {condition && (
        <Input
          className="w-full font-mono text-xs"
          placeholder={
            condition.kind === "env"
              ? "CI"
              : condition.kind === "file_exists"
                ? "path/to/file"
                : "${param} == 'production'"
          }
          value={condition.value}
          onChange={(e) => handleValueChange(e.target.value)}
        />
      )}

      {condition && (
        <div className="text-[10px] text-muted-foreground">
          {condition.kind === "env" && "Stage runs only if this env var is set and non-empty."}
          {condition.kind === "file_exists" && "Stage runs only if this file exists."}
          {condition.kind === "expr" && "Stage runs only if this expression evaluates to true."}
        </div>
      )}
    </div>
  );
}
