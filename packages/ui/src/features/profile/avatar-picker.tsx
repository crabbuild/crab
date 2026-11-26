import { PencilIcon, RotateCcwIcon, ShuffleIcon } from "lucide-react";
import { useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import {
  AVATAR_PALETTES,
  AVATAR_SEED_SUGGESTIONS,
  AVATAR_VARIANTS,
  resolveAvatar,
  UserAvatar,
} from "@/components/ui/user-avatar";
import type {
  AvatarConfig,
  AvatarResponse,
  AvatarVariant,
} from "@/lib/contracts";
import { cn } from "@/lib/utils";

function normalizeHex(value: string) {
  return /^#[0-9a-fA-F]{6}$/.test(value) ? value.toUpperCase() : "#92A1C6";
}

export function ProfileAvatarPicker({
  fallbackName,
  avatar,
  csrf,
  onSaved,
}: {
  fallbackName: string;
  avatar?: AvatarConfig | null;
  csrf: string;
  onSaved: (next: AvatarConfig | null) => void;
}) {
  const [open, setOpen] = useState(false);
  const resolved = resolveAvatar(fallbackName, avatar);
  const [variant, setVariant] = useState<AvatarVariant>(resolved.variant);
  const [seed, setSeed] = useState(resolved.seed);
  const [colors, setColors] = useState<string[]>(resolved.colors);
  const [statusMessage, setStatusMessage] = useState("");
  const mutation = useMutation(csrf, null);

  function openEditor() {
    const current = resolveAvatar(fallbackName, avatar);
    setVariant(current.variant);
    setSeed(current.seed);
    setColors([...current.colors]);
    setStatusMessage("");
    mutation.reset();
    setOpen(true);
  }

  function randomize() {
    const nextSeed =
      AVATAR_SEED_SUGGESTIONS[
        Math.floor(Math.random() * AVATAR_SEED_SUGGESTIONS.length)
      ] ?? "Maria Mitchell";
    const nextVariant =
      AVATAR_VARIANTS[Math.floor(Math.random() * AVATAR_VARIANTS.length)]
        ?.value ?? "beam";
    const nextPalette =
      AVATAR_PALETTES[Math.floor(Math.random() * AVATAR_PALETTES.length)] ??
      AVATAR_PALETTES[0];
    setSeed(nextSeed);
    setVariant(nextVariant);
    setColors([...nextPalette.colors]);
    setStatusMessage("");
  }

  async function saveAvatar(next: AvatarConfig | null) {
    setStatusMessage("");
    const response = await mutation.run<AvatarResponse>(
      "/api/profile/avatar",
      "PUT",
      { avatar: next },
    );
    if (!response) return;
    onSaved(response.avatar ?? null);
    setStatusMessage(
      response.avatar ? "Profile picture updated." : "Profile picture reset.",
    );
    setOpen(false);
  }

  const draft: AvatarConfig = {
    variant,
    seed: seed.trim() || fallbackName.trim() || "Crab User",
    colors: colors.map(normalizeHex),
  };

  return (
    <div className="grid justify-items-start gap-3">
      <h2 className="m-0 text-sm font-semibold text-foreground">
        Profile picture
      </h2>

      <div className="relative">
        <UserAvatar
          name={fallbackName}
          avatar={avatar}
          size={176}
          className="border-2 border-border shadow-xs"
        />
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={openEditor}
          className="absolute bottom-2 left-2 h-8 gap-1.5 rounded-md bg-background/95 px-2.5 text-xs font-medium shadow-sm backdrop-blur-xs hover:bg-accent"
        >
          <PencilIcon className="size-3.5" aria-hidden="true" />
          Edit
        </Button>
      </div>

      <p className="m-0 text-xs leading-relaxed text-muted-foreground">
        Powered by Boring Avatars. Customize the style, seed name, and color
        palette shown across Crab.
      </p>

      {statusMessage && (
        <p role="status" className="m-0 text-xs font-medium text-foreground">
          {statusMessage}
        </p>
      )}

      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>Customize profile picture</DialogTitle>
            <DialogDescription>
              Choose a Boring Avatars style, enter any seed name (for example{" "}
              <span className="font-medium text-foreground">
                Maria Mitchell
              </span>
              ), and pick your palette.
            </DialogDescription>
          </DialogHeader>

          <div className="grid gap-4 py-1">
            <div className="flex flex-wrap items-center justify-between gap-3 rounded-xl border border-border bg-muted/35 p-3">
              <div className="flex items-center gap-3.5">
                <UserAvatar
                  name={fallbackName}
                  avatar={draft}
                  size={72}
                  className="border-2 border-border shadow-xs"
                />
                <div className="grid gap-1.5">
                  <span className="text-xs font-medium text-muted-foreground">
                    Live preview
                  </span>
                  <div className="flex items-center gap-2">
                    <UserAvatar name={fallbackName} avatar={draft} size={36} />
                    <UserAvatar name={fallbackName} avatar={draft} size={26} />
                    <UserAvatar name={fallbackName} avatar={draft} size={20} />
                  </div>
                </div>
              </div>
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={randomize}
                className="gap-1.5"
              >
                <ShuffleIcon className="size-3.5" aria-hidden="true" />
                Randomize
              </Button>
            </div>

            <div className="grid gap-1.5">
              <label
                htmlFor="avatar-seed-input"
                className="text-sm font-medium text-foreground"
              >
                Avatar name
              </label>
              <Input
                id="avatar-seed-input"
                value={seed}
                onChange={(event) => setSeed(event.target.value)}
                placeholder="Maria Mitchell"
                maxLength={100}
              />
              <p className="m-0 text-xs text-muted-foreground">
                Any name or phrase deterministically generates a unique SVG
                avatar.
              </p>
            </div>

            <div className="grid gap-2">
              <span className="text-sm font-medium text-foreground">Style</span>
              <div
                role="radiogroup"
                aria-label="Avatar style"
                className="grid grid-cols-3 gap-2 sm:grid-cols-6"
              >
                {AVATAR_VARIANTS.map(({ value, label }) => {
                  const selected = variant === value;
                  return (
                    <button
                      key={value}
                      type="button"
                      role="radio"
                      aria-checked={selected}
                      onClick={() => setVariant(value)}
                      className={cn(
                        "flex flex-col items-center gap-1.5 rounded-lg border px-1.5 py-2 text-xs font-medium transition-colors focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none",
                        selected
                          ? "border-(--info) bg-accent/60 text-foreground ring-1 ring-(--info)"
                          : "border-border bg-card text-muted-foreground hover:border-input-hover hover:text-foreground",
                      )}
                    >
                      <UserAvatar
                        name={draft.seed}
                        avatar={{ ...draft, variant: value }}
                        size={32}
                      />
                      <span>{label}</span>
                    </button>
                  );
                })}
              </div>
            </div>

            <div className="grid gap-2">
              <span className="text-sm font-medium text-foreground">
                Color palette
              </span>
              <div
                role="radiogroup"
                aria-label="Avatar color palette"
                className="grid grid-cols-2 gap-2 sm:grid-cols-4"
              >
                {AVATAR_PALETTES.map((palette) => {
                  const selected =
                    palette.colors.length === colors.length &&
                    palette.colors.every(
                      (hex, idx) =>
                        hex.toUpperCase() === colors[idx]?.toUpperCase(),
                    );
                  return (
                    <button
                      key={palette.name}
                      type="button"
                      role="radio"
                      aria-checked={selected}
                      onClick={() => setColors([...palette.colors])}
                      className={cn(
                        "grid gap-1.5 rounded-lg border p-2 text-left text-xs font-medium transition-colors focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none",
                        selected
                          ? "border-(--info) bg-accent/60 text-foreground ring-1 ring-(--info)"
                          : "border-border bg-card text-muted-foreground hover:border-input-hover hover:text-foreground",
                      )}
                    >
                      <span
                        className="flex h-3.5 w-full overflow-hidden rounded-sm border border-border/50"
                        aria-hidden="true"
                      >
                        {palette.colors.map((hex) => (
                          <span
                            key={hex}
                            className="flex-1"
                            style={{ backgroundColor: hex }}
                          />
                        ))}
                      </span>
                      <span className="truncate">{palette.name}</span>
                    </button>
                  );
                })}
              </div>

              <div className="grid grid-cols-5 gap-1.5 pt-0.5">
                {colors.map((hex, index) => (
                  <label
                    key={index}
                    className="inline-flex cursor-pointer items-center justify-center gap-1 rounded-md border border-border bg-card px-1.5 py-1 text-[11px] font-mono text-muted-foreground hover:border-input-hover"
                  >
                    <input
                      type="color"
                      aria-label={`Avatar color ${index + 1}`}
                      value={normalizeHex(hex)}
                      onChange={(event) => {
                        const next = [...colors];
                        next[index] = event.target.value.toUpperCase();
                        setColors(next);
                      }}
                      className="size-3.5 shrink-0 cursor-pointer border-0 bg-transparent p-0"
                    />
                    <span className="truncate">{normalizeHex(hex)}</span>
                  </label>
                ))}
              </div>
            </div>

            {mutation.error && (
              <p role="alert" className="m-0 text-sm text-destructive">
                {mutation.error}
              </p>
            )}
          </div>

          <DialogFooter className="flex-wrap justify-between gap-2 sm:justify-between">
            <Button
              type="button"
              variant="ghost"
              size="sm"
              disabled={mutation.pending || !avatar}
              onClick={() => void saveAvatar(null)}
              className="gap-1.5 text-muted-foreground"
            >
              <RotateCcwIcon className="size-3.5" aria-hidden="true" />
              Reset to default
            </Button>
            <div className="flex items-center gap-2">
              <Button
                type="button"
                variant="outline"
                disabled={mutation.pending}
                onClick={() => setOpen(false)}
              >
                Cancel
              </Button>
              <Button
                type="button"
                disabled={mutation.pending || !seed.trim()}
                onClick={() => void saveAvatar(draft)}
              >
                {mutation.pending ? "Saving…" : "Save avatar"}
              </Button>
            </div>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
