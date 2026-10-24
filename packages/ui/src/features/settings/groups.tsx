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
    <FieldSet
      disabled={disabled}
      className="member-group-access min-w-0"
      aria-describedby={`${id}-description ${id}-help`}
    >
      <FieldLegend>SSO group access</FieldLegend>
      <FieldDescription id={`${id}-description`} className="max-w-prose">
        Use the exact group ID or path from your identity provider’s groups
        claim. When someone has multiple grants, their highest role applies.
      </FieldDescription>
      {value.length > 0 && (
        <FieldGroup className="gap-4">
          {value.map((grant, index) => (
            <FieldGroup
              key={index}
              className="grid grid-cols-[minmax(0,1fr)_auto] items-end gap-4 sm:grid-cols-[minmax(0,1fr)_auto_auto]"
            >
              <Field className="col-span-2 min-w-0 sm:col-span-1">
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
              <Field className="min-w-0 sm:w-36">
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
                className="justify-self-end"
                aria-label={`Remove SSO group ${index + 1}`}
                title={`Remove SSO group ${index + 1}`}
                onClick={() => onChange(value.filter((_, i) => i !== index))}
              >
                <TrashIcon />
              </Button>
            </FieldGroup>
          ))}
        </FieldGroup>
      )}
      <Button
        variant="outline"
        className="self-start"
        disabled={disabled || value.length >= 100}
        onClick={() => onChange([...value, { group: "", access: "read" }])}
      >
        <PlusIcon data-icon="inline-start" /> Add SSO group
      </Button>
      <FieldDescription id={`${id}-help`} className="max-w-prose">
        Keep an individual administrator for recovery. Group membership is
        refreshed at sign-in; sessions with groups last up to 15 minutes.
      </FieldDescription>
    </FieldSet>
  );
}
