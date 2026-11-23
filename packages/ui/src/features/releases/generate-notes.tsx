import { useEffect, useRef, useState } from "react";

import { Failure } from "@/components/discussion/failure";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { Button } from "@/components/ui/button";
import { endpoint, request, type Ref, type Repository } from "@/lib/api";
import { refName } from "./api";

interface GeneratedNotes {
  target_oid: string;
  previous_tag: string | null;
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
        <select
          id="release-previous-tag"
          value={previous}
          onChange={(event) => {
            setPrevious(event.target.value);
            setPreview(undefined);
            setError(undefined);
          }}
          disabled={pending || generating}
        >
          <option value="">Automatic</option>
          <option value="refs/tags/">All history</option>
          {tags
            .filter((ref) => refName(ref) !== tag)
            .map((ref) => (
              <option key={ref.name} value={ref.name}>
                {refName(ref)}
              </option>
            ))}
        </select>
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
          <details className="min-w-0">
            <summary>
              {preview.previous_tag
                ? `Preview notes since ${preview.previous_tag}`
                : "Preview notes for all history"}
            </summary>
            <DiscussionMarkdown>{preview.body}</DiscussionMarkdown>
          </details>
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
