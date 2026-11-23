import { useId } from "react";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";

type Prompt = { label: string; description: string | null };
type Text = Prompt & {
  placeholder: string | null;
  value: string | null;
  render: string | null;
};
type Field<T extends string, A> = {
  type: T;
  attributes: A;
  validations: { required: boolean };
};
export type FormField =
  | { type: "markdown"; attributes: { value: string } }
  | Field<"input" | "textarea", Text>
  | Field<
      "dropdown",
      Prompt & { options: string[]; default: number | null; multiple: boolean }
    >
  | Field<
      "checkboxes",
      Prompt & { options: Array<{ label: string; required: boolean }> }
    >;
export type Answer = string | number[];

export function initialAnswers(fields: FormField[]): Answer[] {
  return fields.map((field) => {
    if (field.type === "input" || field.type === "textarea")
      return field.attributes.value ?? "";
    if (field.type === "dropdown" && field.attributes.default !== null)
      return [field.attributes.default];
    return [];
  });
}

export function completeForm(fields: FormField[], answers: Answer[]) {
  return fields.every((field, i) => {
    if (field.type === "markdown") return true;
    const answer = answers[i];
    if (typeof answer === "string")
      return !field.validations.required || Boolean(answer.trim());
    if (
      field.type === "checkboxes" &&
      !field.attributes.options.every(
        (option, index) => !option.required || answer.includes(index),
      )
    )
      return false;
    return !field.validations.required || answer.length > 0;
  });
}

function markdownText(value: string) {
  return value.replace(/[\\`*_\[\]<>#&]/g, "\\$&");
}

export function formMarkdown(fields: FormField[], answers: Answer[]) {
  return fields
    .flatMap((field, i) => {
      if (field.type === "markdown") return [];
      const answer = answers[i];
      let value = "";
      if (field.type === "checkboxes" && Array.isArray(answer)) {
        value = field.attributes.options
          .map(
            (option, index) =>
              `- [${answer.includes(index) ? "x" : " "}] ${option.label}`,
          )
          .join("\n");
      } else if (field.type === "dropdown" && Array.isArray(answer)) {
        value = answer
          .map((index) => markdownText(field.attributes.options[index]))
          .join(", ");
      } else if (
        (field.type === "input" || field.type === "textarea") &&
        typeof answer === "string"
      ) {
        const text = answer.trim();
        if (text && field.attributes.render) {
          const longest = Math.max(
            2,
            ...Array.from(text.matchAll(/`+/g), (match) => match[0].length),
          );
          const fence = "`".repeat(longest + 1);
          value = `${fence}${field.attributes.render}\n${text}\n${fence}`;
        } else value = text;
      }
      return [
        `### ${markdownText(field.attributes.label)}\n\n${value || "_No response_"}`,
      ];
    })
    .join("\n\n");
}

export function TemplateFields({
  fields,
  answers,
  onAnswer,
  disabled,
}: {
  fields: FormField[];
  answers: Answer[];
  onAnswer: (index: number, value: Answer) => void;
  disabled: boolean;
}) {
  const prefix = useId();
  return (
    <div className="grid gap-5">
      {fields.map((field, index) => {
        const id = `${prefix}-${index}`;
        if (field.type === "markdown")
          return (
            <DiscussionMarkdown key={id}>
              {field.attributes.value}
            </DiscussionMarkdown>
          );
        const attributes = field.attributes;
        const answer = answers[index];
        const required = field.validations.required;
        const description = `${id}-description`;
        const help = attributes.description && (
          <div className="muted" id={description}>
            <DiscussionMarkdown>{attributes.description}</DiscussionMarkdown>
          </div>
        );
        const label = (
          <>
            {attributes.label}
            {required && <span className="muted"> (required)</span>}
          </>
        );
        const accessibility = {
          "aria-describedby": attributes.description ? description : undefined,
        };
        if (
          field.type === "checkboxes" ||
          (field.type === "dropdown" && field.attributes.multiple)
        ) {
          const checks =
            field.type === "checkboxes"
              ? field.attributes.options
              : field.attributes.options.map((value) => ({
                  label: value,
                  required: false,
                }));
          const selected = Array.isArray(answer) ? answer : [];
          return (
            <fieldset
              key={id}
              disabled={disabled}
              {...accessibility}
              className="grid gap-2"
            >
              <legend className="font-medium">{label}</legend>
              {help}
              {checks.map((option, optionIndex) => (
                <div key={optionIndex} className="flex items-start gap-2">
                  <input
                    className="mt-1"
                    type="checkbox"
                    id={`${id}-${optionIndex}`}
                    checked={selected.includes(optionIndex)}
                    required={option.required}
                    onChange={(event) =>
                      onAnswer(
                        index,
                        event.target.checked
                          ? [...selected, optionIndex].sort((a, b) => a - b)
                          : selected.filter((value) => value !== optionIndex),
                      )
                    }
                  />
                  <label htmlFor={`${id}-${optionIndex}`}>
                    <DiscussionMarkdown>{option.label}</DiscussionMarkdown>
                    {option.required && (
                      <span className="muted"> Required</span>
                    )}
                  </label>
                </div>
              ))}
            </fieldset>
          );
        }
        const props = { id, disabled, required, ...accessibility };
        let control;
        if (field.type === "dropdown") {
          const selected = Array.isArray(answer) ? answer[0] : undefined;
          control = (
            <NativeSelect
              {...props}
              value={selected ?? ""}
              onChange={(event) =>
                onAnswer(
                  index,
                  event.target.value === "" ? [] : [Number(event.target.value)],
                )
              }
            >
              <NativeSelectOption value="">Choose an option</NativeSelectOption>
              {field.attributes.options.map((option, index) => (
                <NativeSelectOption key={index} value={index}>
                  {option}
                </NativeSelectOption>
              ))}
            </NativeSelect>
          );
        } else {
          const text = {
            ...props,
            value: typeof answer === "string" ? answer : "",
            maxLength: 65536,
            placeholder: field.attributes.placeholder ?? undefined,
            onChange: (
              event: React.ChangeEvent<HTMLInputElement | HTMLTextAreaElement>,
            ) => onAnswer(index, event.target.value),
          };
          control =
            field.type === "textarea" ? (
              <Textarea {...text} rows={5} />
            ) : (
              <Input {...text} />
            );
        }
        return (
          <div className="grid gap-2" key={id}>
            <label htmlFor={id} className="font-medium">
              {label}
            </label>
            {help}
            {control}
          </div>
        );
      })}
    </div>
  );
}
