import BoringAvatar from "boring-avatars";
import type { AvatarConfig, AvatarVariant } from "@/lib/contracts";
import { cn } from "@/lib/utils";

export const AVATAR_VARIANTS: ReadonlyArray<{
  value: AvatarVariant;
  label: string;
}> = [
  { value: "beam", label: "Beam" },
  { value: "marble", label: "Marble" },
  { value: "pixel", label: "Pixel" },
  { value: "sunset", label: "Sunset" },
  { value: "ring", label: "Ring" },
  { value: "bauhaus", label: "Bauhaus" },
];

export const AVATAR_PALETTES: ReadonlyArray<{
  name: string;
  colors: readonly [string, string, string, string, string];
}> = [
  {
    name: "Classic",
    colors: ["#92A1C6", "#146A7C", "#F0AB3D", "#C271B4", "#C20D90"],
  },
  {
    name: "Ocean",
    colors: ["#0A0310", "#49007E", "#FF005B", "#FF7D10", "#FFB238"],
  },
  {
    name: "Meadow",
    colors: ["#264653", "#2A9D8F", "#E9C46A", "#F4A261", "#E76F51"],
  },
  {
    name: "Pastel",
    colors: ["#A3A948", "#EDB92E", "#F85931", "#CE1836", "#009989"],
  },
  {
    name: "Nordic",
    colors: ["#1B263B", "#415A77", "#778DA9", "#E0E1DD", "#F4A261"],
  },
  {
    name: "Coral",
    colors: ["#FFADAD", "#FFD6A5", "#FDFFB6", "#CAFFBF", "#9BF6FF"],
  },
  {
    name: "Ember",
    colors: ["#590D22", "#A4133C", "#FF4D6D", "#FF8FA3", "#FFF0F3"],
  },
  {
    name: "Forest",
    colors: ["#132A13", "#31572C", "#4F772D", "#90A955", "#ECF39E"],
  },
];

export const DEFAULT_AVATAR_COLORS: string[] = [...AVATAR_PALETTES[0].colors];

export const AVATAR_SEED_SUGGESTIONS: readonly string[] = [
  "Maria Mitchell",
  "Grace Hopper",
  "Ada Lovelace",
  "Katherine Johnson",
  "Rosalind Franklin",
  "Margaret Hamilton",
  "Hedy Lamarr",
  "Alan Turing",
  "Radia Perlman",
  "Barbara Liskov",
  "Frances Allen",
  "Mary Jackson",
];

export function resolveAvatar(
  fallbackName: string,
  avatar?: AvatarConfig | null,
): AvatarConfig {
  const seed = avatar?.seed?.trim() || fallbackName.trim() || "Crab User";
  const variant: AvatarVariant = avatar?.variant ?? "beam";
  const colors =
    avatar?.colors && avatar.colors.length > 0
      ? avatar.colors
      : DEFAULT_AVATAR_COLORS;
  return { variant, seed, colors };
}

// Organizations have no stored picture. Seeding by the stable UUID keeps the
// square identicon unchanged across renames, like a GitHub organization avatar.
export function OrganizationAvatar({
  id,
  size = 32,
  className,
}: {
  id: string;
  size?: number;
  className?: string;
}) {
  return (
    <UserAvatar
      name={id}
      avatar={{ variant: "pixel", seed: id, colors: DEFAULT_AVATAR_COLORS }}
      size={size}
      square
      className={className}
    />
  );
}

export function UserAvatar({
  name,
  avatar,
  size = 32,
  square = false,
  className,
  title,
}: {
  name: string;
  avatar?: AvatarConfig | null;
  size?: number;
  square?: boolean;
  className?: string;
  title?: string;
}) {
  const resolved = resolveAvatar(name, avatar);
  return (
    <span
      className={cn(
        "inline-flex shrink-0 items-center justify-center overflow-hidden border border-border/60 bg-muted select-none",
        square ? "rounded-md" : "rounded-full",
        className,
      )}
      style={{ width: size, height: size }}
      title={title}
      aria-hidden="true"
    >
      <BoringAvatar
        size={size}
        name={resolved.seed}
        variant={resolved.variant}
        colors={resolved.colors}
        square={square}
      />
    </span>
  );
}
