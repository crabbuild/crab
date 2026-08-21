import type { LucideIcon } from "lucide-react"
import {
  Bot,
  Bug,
  Copy,
  Database,
  Download,
  Droplets,
  FolderGit,
  Globe,
  HardDrive,
  Layers,
  Plus,
  BookOpen,
  FileSearch,
  FolderOpen,
  Library,
  Lock,
  PenTool,
  Play,
  Rocket,
  Search,
  Settings,
  ShieldCheck,
  Terminal,
  Upload,
  Wind,
  Workflow,
  Wrench,
  Zap,
} from "lucide-react"

/**
 * Icon mapping for all docs sidebar items (sections and sub-sections).
 * Keys are slug paths matching the sidebar navigation structure.
 */
export const docsSidebarIcons: Record<string, LucideIcon> = {
  // Top-level sections
  architecture: Layers,
  design: PenTool,
  guides: BookOpen,
  reference: Library,
  workflow: Workflow,
  workflows: Workflow,
  "getting-started": Rocket,
  "daily-workflow": FolderOpen,
  "file-inspection": FileSearch,
  authentication: ShieldCheck,
  storage: HardDrive,
  "cache-service": Database,
  automation: Play,
  agent: Bot,
  "virtual-filesystem": Globe,
  diagnostics: Wrench,
  installation: Download,
  configuration: Settings,
  commands: Terminal,
  troubleshooting: Bug,

  // Sub-sections: getting-started
  "getting-started/quickstart": Zap,
  "getting-started/first-repo": FolderGit,

  // Sub-sections: commands
  "commands/add": Plus,
  "commands/push": Upload,
  "commands/clone": Copy,
  "commands/hydrate": Droplets,
  "commands/dehydrate": Wind,

  // Sub-sections: configuration
  "configuration/auth": ShieldCheck,
  "configuration/cache": HardDrive,
  "configuration/remotes": Globe,
}

/**
 * Returns the Lucide icon component for a given sidebar slug,
 * or undefined if no icon is mapped for that slug.
 */
export function getSidebarIcon(slug: string): LucideIcon | undefined {
  return docsSidebarIcons[slug]
}
