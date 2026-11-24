import { Disclosure } from "@/components/ui/collapsible";
import { FieldSelect } from "@/components/ui/field-select";
import { useEffect, useRef, useState } from "react";

import { Failure } from "@/components/discussion/failure";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { Button } from "@/components/ui/button";
import { endpoint, request, type Ref, type Repository } from "@/lib/api";
import { refName } from "./api";

interface GeneratedNotes {
  target_oid: string;
  previous_tag: string | null;
  configuration_file: string | null;
  body: string;
}

export function ReleaseNotesGenerator({
  repo,
  tag,
  tags,
  target,
  pending,
  append,
}: {
  repo: Repository;
  tag: string;
  tags: Ref[];
  target: string;
  pending: boolean;
  append: (notes: string) => void;
}) {
  const [previous, setPrevious] = useState("");
  const [generating, setGenerating] = useState(false);
  const [preview, setPreview] = useState<GeneratedNotes>();
  const [error, setError] = useState<string>();
  const current = useRef<AbortController>(null);
  useEffect(() => () => current.current?.abort(), []);

  const generate = async () => {
    const controller = new AbortController();
    current.current = controller;
    setGenerating(true);
    setPreview(undefined);
    setError(undefined);
    try {
      const { data } = await request<GeneratedNotes>(
        endpoint(repo, "releases/generate-notes", {
          target_oid: target,
          previous_tag: previous
            ? previous.replace(/^refs\/tags\//, "")
            : undefined,
        }),
        controller.signal,
        repo.id,
      );
      if (controller.signal.aborted || data.target_oid !== target) return;
      setPreview(data);
    } catch (failure) {
      if (controller.signal.aborted) return;
      setError(
        failure instanceof Error
          ? failure.message
          : "Release notes could not be generated",
      );
    } finally {
      if (!controller.signal.aborted) setGenerating(false);
    }
  };

  return (
    <div className="grid gap-2">
      <div className="flex flex-wrap items-center gap-2">
        <label htmlFor="release-previous-tag">Previous tag</label>
        <FieldSelect
          id="release-previous-tag"
          value={previous}
          onValueChange={(nextValue) => {
            setPrevious(nextValue);
            setPreview(undefined);
            setError(undefined);
          }}
          disabled={pending || generating}
          options={[
            { value: "", label: "Automatic" },
            { value: "refs/tags/", label: "All history" },
            ...tags
              .filter((ref) => refName(ref) !== tag)
              .map((ref) => ({ value: ref.name, label: refName(ref) })),
          ]}
        />
        <Button
          type="button"
          size="sm"
          variant="outline"
          disabled={pending || generating}
          onClick={() => void generate()}
        >
          {generating ? "Generating…" : "Generate release notes"}
        </Button>
      </div>
      <Failure message={error} />
      {preview && (
        <>
          <Disclosure
            className="min-w-0"
            summary={
              <>
                {preview.previous_tag
                  ? `Preview notes since ${preview.previous_tag}`
                  : "Preview notes for all history"}
              </>
            }
          >
            {preview.configuration_file && (
              <p className="text-sm text-muted-foreground">
                Rules from <code>{preview.configuration_file}</code>
              </p>
            )}
            <DiscussionMarkdown>{preview.body}</DiscussionMarkdown>
          </Disclosure>
          <Button
            type="button"
            size="sm"
            variant="outline"
            className="justify-self-start"
            disabled={pending}
            onClick={() => {
              append(preview.body);
              setPreview(undefined);
            }}
          >
            Insert generated notes
          </Button>
        </>
      )}
    </div>
  );
}
