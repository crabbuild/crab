import {
  Braces,
  Box,
  CircleDot,
  Code2,
  Diamond,
  Hash,
  Layers,
  type LucideIcon,
  Package,
  Variable,
} from "lucide-react";
import type { SymbolKind } from "../lib/symbol-tree";

const KIND_ICON_MAP: Record<SymbolKind, LucideIcon> = {
  function: Code2,
  method: Braces,
  class: Diamond,
  interface: CircleDot,
  enum: Hash,
  variable: Variable,
  constant: Layers,
  module: Package,
  struct: Box,
  trait: CircleDot,
  impl: Diamond,
};

const KIND_COLOR_MAP: Record<SymbolKind, string> = {
  function: "text-gx-warn",
  method: "text-gx-warn",
  class: "text-gx-info",
  interface: "text-sky-400",
  enum: "text-orange-400",
  variable: "text-emerald-400",
  constant: "text-emerald-500",
  module: "text-gx-thinking",
  struct: "text-gx-info",
  trait: "text-sky-500",
  impl: "text-blue-300",
};

interface SymbolKindIconProps {
  kind: SymbolKind;
  size?: number;
  className?: string;
}

/**
 * Renders a lucide-react icon appropriate for the given symbol kind,
 * with a color hint to distinguish symbol types at a glance.
 */
export function SymbolKindIcon({ kind, size = 14, className }: SymbolKindIconProps) {
  const Icon = KIND_ICON_MAP[kind];
  const color = KIND_COLOR_MAP[kind];
  return <Icon size={size} className={className ?? color} />;
}
