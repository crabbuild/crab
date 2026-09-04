import { useEffect, useState, type FormEvent } from "react";
import { Button, TextInput } from "@primer/react";
import { SearchIcon } from "@primer/octicons-react";

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
      className="discussion-search"
      role="search"
      aria-label={label}
      onSubmit={submit}
    >
      <div className="discussion-search-field">
        <TextInput
          block
          leadingVisual={SearchIcon}
          aria-label={label}
          placeholder={placeholder}
          maxLength={256}
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
        />
      </div>
      <Button
        className="discussion-search-submit"
        type="submit"
        size="small"
        aria-label="Search"
        title={label}
      >
        <SearchIcon />
      </Button>
      {value && (
        <Button
          type="button"
          size="small"
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
