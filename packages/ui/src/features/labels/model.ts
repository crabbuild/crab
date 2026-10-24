import type { RepositoryLabel } from "@/lib/api";

export interface LabelCatalog {
  items: RepositoryLabel[];
  can_manage: boolean;
}
