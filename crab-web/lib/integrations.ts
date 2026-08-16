export type IntegrationCategory = "cloud" | "ci-cd" | "ml" | "vcs"

export interface Integration {
  id: string
  name: string
  description: string
  category: IntegrationCategory
  icon: string
  href: string
}

/**
 * Returns integrations matching the given category. When category is an
 * empty string, returns all integrations unchanged.
 */
export function filterIntegrations(
  category: string,
  integrations: Integration[]
): Integration[] {
  if (!category) return integrations
  return integrations.filter((i) => i.category === category)
}

export const categoryLabels: Record<IntegrationCategory, string> = {
  cloud: "Cloud Providers",
  "ci-cd": "CI/CD",
  ml: "ML Frameworks",
  vcs: "Version Control",
}

export const integrations: Integration[] = [
  {
    id: "aws-s3",
    name: "AWS S3",
    description:
      "Store repositories in Amazon S3 with intelligent tiering, cross-region replication, and IAM-based access control.",
    category: "cloud",
    icon: "Cloud",
    href: "/docs/cli/storage/aws-s3",
  },
  {
    id: "gcs",
    name: "Google Cloud Storage",
    description:
      "Use GCS buckets as your Crab remote with multi-region redundancy and fine-grained IAM permissions.",
    category: "cloud",
    icon: "Cloud",
    href: "/docs/cli/storage/gcs",
  },
  {
    id: "azure-blob",
    name: "Azure Blob Storage",
    description:
      "Connect to Azure Blob containers with support for hot, cool, and archive tiers via Crab remotes.",
    category: "cloud",
    icon: "Cloud",
    href: "/docs/cli/storage/azure",
  },
  {
    id: "github-actions",
    name: "GitHub Actions",
    description:
      "Automate large-file workflows in CI with crab push/pull steps, caching, and dedup-aware artifact uploads.",
    category: "ci-cd",
    icon: "GitBranch",
    href: "https://docs.github.com/en/actions",
  },
  {
    id: "gitlab-ci",
    name: "GitLab CI",
    description:
      "Integrate Crab into GitLab pipelines for efficient large-file handling with shared runner caching.",
    category: "ci-cd",
    icon: "GitBranch",
    href: "https://docs.gitlab.com/ee/ci/",
  },
  {
    id: "dvc",
    name: "DVC",
    description:
      "Migrate from DVC to Crab or use both side-by-side — Crab reads DVC-tracked pointer files natively.",
    category: "ml",
    icon: "Database",
    href: "https://dvc.org/doc",
  },
  {
    id: "mlflow",
    name: "MLflow",
    description:
      "Version ML artifacts alongside code using Crab's chunk-level dedup, then log runs to MLflow tracking.",
    category: "ml",
    icon: "FlaskConical",
    href: "https://mlflow.org/docs/latest/index.html",
  },
  {
    id: "wandb",
    name: "Weights & Biases",
    description:
      "Track experiments with W&B while Crab handles large model checkpoints and dataset versioning.",
    category: "ml",
    icon: "LineChart",
    href: "https://docs.wandb.ai/",
  },
  {
    id: "huggingface",
    name: "Hugging Face",
    description:
      "Push and pull large model files to Hugging Face Hub repos with Crab's dedup and lazy checkout.",
    category: "ml",
    icon: "Bot",
    href: "https://huggingface.co/docs",
  },
  {
    id: "vscode",
    name: "VS Code",
    description:
      "Use Crab seamlessly from VS Code's integrated terminal and source control panel with full Git UX.",
    category: "vcs",
    icon: "Code",
    href: "https://code.visualstudio.com/",
  },
  {
    id: "jetbrains",
    name: "JetBrains IDEs",
    description:
      "IntelliJ, PyCharm, and other JetBrains IDEs work with Crab via standard Git integration — no plugins needed.",
    category: "vcs",
    icon: "Laptop",
    href: "https://www.jetbrains.com/",
  },
  {
    id: "docker",
    name: "Docker",
    description:
      "Build container images with large assets managed by Crab — hydrate only what the build needs.",
    category: "ci-cd",
    icon: "Container",
    href: "https://docs.docker.com/",
  },
]
