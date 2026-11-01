import { useId } from "react";
import {
  Field,
  FieldDescription,
  FieldGroup,
  FieldLabel,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { FieldSelect } from "@/components/ui/field-select";
import type { Draft, Visibility } from "./model";
export function TeamFields({
  draft,
  onChange,
  disabled,
}: {
  draft: Draft;
  onChange: (draft: Draft) => void;
  disabled: boolean;
}) {
  const id = useId();
  return (
    <FieldGroup>
      <Field>
        <FieldLabel htmlFor={`${id}-name`}>Team name</FieldLabel>
        <Input
          id={`${id}-name`}
          value={draft.name}
          required
          maxLength={100}
          disabled={disabled}
          onChange={(event) => onChange({ ...draft, name: event.target.value })}
        />
      </Field>
      <Field>
        <FieldLabel htmlFor={`${id}-description`}>Description</FieldLabel>
        <Textarea
          id={`${id}-description`}
          value={draft.description}
          maxLength={1000}
          disabled={disabled}
          onChange={(event) =>
            onChange({ ...draft, description: event.target.value })
          }
        />
        <FieldDescription>What does this team do?</FieldDescription>
      </Field>
      <Field>
        <FieldLabel htmlFor={`${id}-visibility`}>Team visibility</FieldLabel>
        <FieldSelect
          id={`${id}-visibility`}
          label="Team visibility"
          value={draft.visibility}
          disabled={disabled}
          onValueChange={(visibility) =>
            onChange({ ...draft, visibility: visibility as Visibility })
          }
          options={{ closed: "Visible", secret: "Secret" }}
        />
        <FieldDescription>
          {draft.visibility === "secret"
            ? "Only team members and organization owners can see this team."
            : "All organization members can see this team."}
        </FieldDescription>
      </Field>
    </FieldGroup>
  );
}
