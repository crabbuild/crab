import { createContext } from "react";
import type { Repository } from "@/lib/api";
import type { CodeThemes } from "@/lib/code-theme";

export const DiscussionRepository = createContext<Pick<
  Repository,
  "id" | "owner" | "name"
> | null>(null);
export const DiscussionCodeTheme = createContext<{
  theme: "light" | "dark";
  codeThemes: CodeThemes;
} | null>(null);
export const DiscussionCsrf = createContext<string | null>(null);
