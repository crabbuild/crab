import { useEffect, useState, type FormEvent } from "react";
import { SearchIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupButton,
  InputGroupInput,
} from "@/components/ui/input-group";

export function DiscussionSearch({
  label,
  placeholder,
  value,
  onSearch,
}: {
  label: string;
  placeholder: string;
  value: string;
  onSearch: (value: string) => void;
}) {
  const [draft, setDraft] = useState(value);
  useEffect(() => setDraft(value), [value]);
  const submit = (event: FormEvent) => {
    event.preventDefault();
    onSearch(draft.trim());
  };
  return (
    <form
      className="my-4 flex min-w-0 items-center gap-2"
      role="search"
      aria-label={label}
      onSubmit={submit}
    >
      <InputGroup>
        <InputGroupInput
          aria-label={label}
          placeholder={placeholder}
          maxLength={256}
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
        />
        <InputGroupAddon>
          <SearchIcon aria-hidden="true" />
        </InputGroupAddon>
        <InputGroupAddon align="inline-end">
          <InputGroupButton type="submit" aria-label="Search" title={label}>
            Search
          </InputGroupButton>
        </InputGroupAddon>
      </InputGroup>
      {value && (
        <Button
          type="button"
          variant="outline"
          onClick={() => {
            setDraft("");
            onSearch("");
          }}
        >
          Clear
        </Button>
      )}
    </form>
  );
}
