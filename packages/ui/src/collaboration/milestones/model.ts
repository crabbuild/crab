export interface Milestone {
  number: number;
  title: string;
  description: string;
  state: "open" | "closed";
  due_on: string | null;
  author: string;
  open_count: number;
  closed_count: number;
  version: number;
  created_at: number;
  updated_at: number;
  closed_at: number | null;
}

export interface MilestonePage {
  items: Milestone[];
  next: number | null;
  can_manage: boolean;
}

export interface MilestoneDetail {
  milestone: Milestone;
  can_manage: boolean;
}

export interface MilestoneAssignment {
  milestone: Milestone | null;
  version: number;
  can_manage: boolean;
}
