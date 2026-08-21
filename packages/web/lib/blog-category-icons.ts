import {
  Package,
  GraduationCap,
  Layers,
  Briefcase,
  Rocket,
  type LucideIcon,
} from "lucide-react"

export type BlogCategory =
  | "product"
  | "tutorial"
  | "architecture"
  | "use-case"
  | "release"

export const categoryIcons: Record<BlogCategory, LucideIcon> = {
  product: Package,
  tutorial: GraduationCap,
  architecture: Layers,
  "use-case": Briefcase,
  release: Rocket,
}

export const categoryLabels: Record<BlogCategory, string> = {
  product: "Product",
  tutorial: "Tutorial",
  architecture: "Architecture",
  "use-case": "Use Case",
  release: "Release",
}
