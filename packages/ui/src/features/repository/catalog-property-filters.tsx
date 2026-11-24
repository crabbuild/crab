import { FieldSelect } from "@/components/ui/field-select";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Result } from "@/components/request/result";
import { useRequest } from "@/lib/api";
import {
  PropertyInput,
  propertySchemaPath,
  type PropertySchema,
  type PropertyValue,
} from "@/features/organizations/property-fields";

export function RepositoryPropertyFilters({
  organization,
  apply,
}: {
  organization: string;
  apply: (filter: string) => boolean;
}) {
  const [open, setOpen] = useState(false);
  const [field, setField] = useState("");
  const [value, setValue] = useState<PropertyValue | null>(null);
  const [exclude, setExclude] = useState(false);
  const [tooLong, setTooLong] = useState(false);
  const fields = useRequest<PropertySchema>(
    open ? `${propertySchemaPath(organization)}/filters` : null,
  );

  return (
    <div className="grid gap-2 justify-items-start">
      <Button
        type="button"
        size="sm"
        variant="outline"
        aria-expanded={open}
        onClick={() => {
          setOpen(!open);
          setTooLong(false);
        }}
      >
        Filter by property
      </Button>
      {open && (
        <Result state={fields} timingLabel="Repository property filters">
          {({ definitions }) => {
            const definition = definitions.find((item) => item.id === field);
            if (!definitions.length)
              return <p>No repository properties are available.</p>;
            return (
              <form
                aria-label="Repository property filter"
                className="grid w-full max-w-md gap-3 rounded-lg border p-4"
                onSubmit={(event) => {
                  event.preventDefault();
                  if (!definition || value === null) return;
                  const raw = String(value).trim();
                  if (!raw) return;
                  const term = /\s/.test(raw) ? `"${raw}"` : raw;
                  if (
                    !apply(
                      `${exclude ? "-" : ""}props.${definition.name}:${term}`,
                    )
                  ) {
                    setTooLong(true);
                    return;
                  }
                  setOpen(false);
                  setField("");
                  setValue(null);
                  setExclude(false);
                  setTooLong(false);
                }}
              >
                <label className="grid gap-1">
                  Property
                  <FieldSelect
                    className="w-full"
                    value={field}
                    required
                    onValueChange={(nextValue) => {
                      setField(nextValue);
                      setValue(null);
                    }}
                    options={[
                      { value: "", label: "Choose a property" },
                      ...definitions.map((item) => ({
                        value: item.id,
                        label: item.name,
                      })),
                    ]}
                  />
                </label>
                {definition && (
                  <>
                    <label className="grid gap-1">
                      Match
                      <FieldSelect
                        className="w-full"
                        value={exclude ? "exclude" : "include"}
                        onValueChange={(nextValue) =>
                          setExclude(nextValue === "exclude")
                        }
                        options={[
                          { value: "include", label: "Has this value" },
                          { value: "exclude", label: "Excludes this value" },
                        ]}
                      />
                    </label>
                    <PropertyInput
                      key={field}
                      label="Value"
                      definition={
                        definition.kind === "multi_select"
                          ? { ...definition, kind: "single_select" }
                          : definition
                      }
                      value={value}
                      unset="Choose a value"
                      onChange={setValue}
                    />
                  </>
                )}
                {tooLong && (
                  <p role="alert">
                    Shorten the search to fit this filter within 200 characters.
                  </p>
                )}
                <Button
                  type="submit"
                  size="sm"
                  className="justify-self-start"
                  disabled={
                    !definition || value === null || !String(value).trim()
                  }
                >
                  Add filter
                </Button>
              </form>
            );
          }}
        </Result>
      )}
    </div>
  );
}
