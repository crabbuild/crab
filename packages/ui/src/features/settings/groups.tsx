import { useId } from "react";
import { PlusIcon, TrashIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Field,
  FieldDescription,
  FieldGroup,
  FieldLabel,
  FieldLegend,
  FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import type { RepositoryGroup } from "@/lib/api";

export function GroupAccess({
  value,
  onChange,
  disabled,
}: {
  value: RepositoryGroup[];
  onChange: (groups: RepositoryGroup[]) => void;
  disabled: boolean;
}) {
  const id = useId();
  function change(index: number, next: RepositoryGroup) {
    onChange(value.map((grant, i) => (i === index ? next : grant)));
  }
  return (
    <FieldSet disabled={disabled} className="mt-8 min-w-0">
      <FieldLegend>SSO group access</FieldLegend>
      <FieldDescription>
        Grant access using exact identifiers in your provider’s signed groups
        claim. Matching grants combine with individual access; the highest role
        applies. Keep an individual administrator for recovery. Group changes at
        the identity provider take effect at the next login, within 15 minutes
        at most.
      </FieldDescription>
      <FieldGroup className="gap-4">
        {value.map((grant, index) => (
          <div key={index} className="flex flex-wrap items-end gap-3">
            <Field className="min-w-48 flex-1">
              <FieldLabel htmlFor={`${id}-${index}`}>
                Group identifier {index + 1}
              </FieldLabel>
              <Input
                id={`${id}-${index}`}
                value={grant.group}
                maxLength={512}
                autoComplete="off"
                placeholder="Exact group ID or path"
                onChange={(event) =>
                  change(index, { ...grant, group: event.target.value })
                }
              />
            </Field>
            <Field className="w-36">
              <FieldLabel htmlFor={`${id}-role-${index}`}>Role</FieldLabel>
              <NativeSelect
                id={`${id}-role-${index}`}
                value={grant.access}
                onChange={(event) => {
                  const access = event.target.value;
                  if (
                    access === "read" ||
                    access === "triage" ||
                    access === "write" ||
                    access === "admin"
                  )
                    change(index, { ...grant, access });
                }}
              >
                <NativeSelectOption value="read">Read</NativeSelectOption>
                <NativeSelectOption value="triage">Triage</NativeSelectOption>
                <NativeSelectOption value="write">Write</NativeSelectOption>
                <NativeSelectOption value="admin">Admin</NativeSelectOption>
              </NativeSelect>
            </Field>
            <Button
              variant="ghost"
              size="icon"
              aria-label={`Remove SSO group ${index + 1}`}
              onClick={() => onChange(value.filter((_, i) => i !== index))}
            >
              <TrashIcon />
            </Button>
          </div>
        ))}
      </FieldGroup>
      <Button
        variant="outline"
        className="self-start"
        disabled={disabled || value.length >= 100}
        onClick={() => onChange([...value, { group: "", access: "read" }])}
      >
        <PlusIcon data-icon="inline-start" /> Add SSO group
      </Button>
    </FieldSet>
  );
}
