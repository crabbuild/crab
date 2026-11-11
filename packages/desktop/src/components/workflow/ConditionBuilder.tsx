import { NativeSelect } from "@/components/ui/native-select";
import { Input } from "@/components/ui/input";
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
      <NativeSelect
        className="w-full text-xs"
        value={condition?.kind ?? "none"}
        onChange={(e) => handleKindChange(e.target.value)}
      >
        <option value="none">No condition</option>
        <option value="env">Environment variable</option>
        <option value="file_exists">File exists</option>
        <option value="expr">Expression</option>
      </NativeSelect>

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
