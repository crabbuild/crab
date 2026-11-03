/**
 * Connection Templates — template picker shown in the "Add Connection"
 * dialog as a first step before the form. Provides pre-built configs
 * for common cloud providers and custom user templates.
 */

import { useCallback, useEffect, useState } from "react";

// ── Types ────────────────────────────────────────────────────────

export interface ConnectionTemplate {
  id: string;
  name: string;
  description: string;
  icon: string;
  provider: string;
  defaults: {
    username: string;
    port: number;
    authMethod: { type: string; keyPath?: string };
    forwardAgent: boolean;
    compression: boolean;
    autoReconnect: boolean;
    autoDeployAgent: boolean;
    tags: string[];
  };
  isCustom?: boolean;
}

interface ConnectionTemplatesProps {
  onSelect: (template: ConnectionTemplate) => void;
  onSkip: () => void;
  customTemplates: ConnectionTemplate[];
}

// ── Built-in Templates ───────────────────────────────────────────

const BUILT_IN_TEMPLATES: ConnectionTemplate[] = [
  {
    id: "aws-ec2",
    name: "AWS EC2",
    description: "Amazon EC2 instance (Ubuntu or Amazon Linux)",
    icon: "☁️",
    provider: "aws",
    defaults: {
      username: "ubuntu",
      port: 22,
      authMethod: { type: "key", keyPath: "~/.ssh/id_ed25519" },
      forwardAgent: false,
      compression: false,
      autoReconnect: true,
      autoDeployAgent: true,
      tags: ["aws", "cloud"],
    },
  },
  {
    id: "gcp-compute",
    name: "GCP Compute Engine",
    description: "Google Cloud VM instance",
    icon: "🔷",
    provider: "gcp",
    defaults: {
      username: "",
      port: 22,
      authMethod: { type: "key", keyPath: "~/.ssh/google_compute_engine" },
      forwardAgent: false,
      compression: false,
      autoReconnect: true,
      autoDeployAgent: true,
      tags: ["gcp", "cloud"],
    },
  },
  {
    id: "lambda-labs",
    name: "Lambda Labs",
    description: "Lambda GPU cloud instance",
    icon: "🐑",
    provider: "lambda",
    defaults: {
      username: "ubuntu",
      port: 22,
      authMethod: { type: "key", keyPath: "~/.ssh/id_ed25519" },
      forwardAgent: false,
      compression: false,
      autoReconnect: true,
      autoDeployAgent: true,
      tags: ["gpu", "lambda"],
    },
  },
  {
    id: "runpod",
    name: "RunPod",
    description: "RunPod GPU pod (custom port)",
    icon: "🚀",
    provider: "runpod",
    defaults: {
      username: "root",
      port: 22,
      authMethod: { type: "key", keyPath: "~/.ssh/id_ed25519" },
      forwardAgent: false,
      compression: true,
      autoReconnect: true,
      autoDeployAgent: true,
      tags: ["gpu", "runpod"],
    },
  },
  {
    id: "paperspace",
    name: "Paperspace",
    description: "Paperspace Gradient machine",
    icon: "📐",
    provider: "paperspace",
    defaults: {
      username: "paperspace",
      port: 22,
      authMethod: { type: "key", keyPath: "~/.ssh/id_ed25519" },
      forwardAgent: false,
      compression: false,
      autoReconnect: true,
      autoDeployAgent: true,
      tags: ["gpu", "paperspace"],
    },
  },
  {
    id: "generic-linux",
    name: "Generic Linux",
    description: "Any Linux server with SSH access",
    icon: "🐧",
    provider: "generic",
    defaults: {
      username: "",
      port: 22,
      authMethod: { type: "agent" },
      forwardAgent: false,
      compression: false,
      autoReconnect: true,
      autoDeployAgent: true,
      tags: [],
    },
  },
  {
    id: "macos-build",
    name: "macOS Build Machine",
    description: "macOS server for builds and CI",
    icon: "🍎",
    provider: "macos",
    defaults: {
      username: "",
      port: 22,
      authMethod: { type: "agent" },
      forwardAgent: true,
      compression: false,
      autoReconnect: true,
      autoDeployAgent: true,
      tags: ["macos", "build"],
    },
  },
];

// ── Component ────────────────────────────────────────────────────

export function ConnectionTemplates({
  onSelect,
  onSkip,
  customTemplates,
}: ConnectionTemplatesProps) {
  const allTemplates = [...BUILT_IN_TEMPLATES, ...customTemplates];

  return (
    <div className="ssh-templates" role="dialog" aria-labelledby="ssh-templates-title">
      <header className="ssh-templates__header">
        <h2 id="ssh-templates-title">Choose a template</h2>
        <p className="ssh-templates__subtitle">
          Start with a pre-configured template or set up from scratch.
        </p>
      </header>

      <div className="ssh-templates__grid" role="list" aria-label="Connection templates">
        {allTemplates.map((template) => (
          <button
            key={template.id}
            className={`ssh-templates__card ${template.isCustom ? "ssh-templates__card--custom" : ""}`}
            onClick={() => onSelect(template)}
            role="listitem"
            aria-label={`${template.name}: ${template.description}`}
          >
            <span className="ssh-templates__card-icon" aria-hidden="true">
              {template.icon}
            </span>
            <span className="ssh-templates__card-name">{template.name}</span>
            <span className="ssh-templates__card-desc">{template.description}</span>
            {template.isCustom && (
              <span className="ssh-templates__card-badge">Custom</span>
            )}
          </button>
        ))}
      </div>

      <footer className="ssh-templates__footer">
        <button
          className="ssh-templates__skip"
          onClick={onSkip}
          aria-label="Skip template selection and configure manually"
        >
          Configure manually →
        </button>
      </footer>
    </div>
  );
}

/**
 * Hook to load custom templates from storage.
 */
export function useCustomTemplates(): {
  templates: ConnectionTemplate[];
  saveTemplate: (template: ConnectionTemplate) => void;
  deleteTemplate: (id: string) => void;
} {
  const [templates, setTemplates] = useState<ConnectionTemplate[]>([]);

  useEffect(() => {
    // Load from localStorage (in production, this would use IPC to read
    // from ~/.crab/desktop/ssh-templates.json)
    try {
      const stored = localStorage.getItem("ssh-custom-templates");
      if (stored) {
        setTemplates(JSON.parse(stored));
      }
    } catch {
      // Ignore parse errors
    }
  }, []);

  const saveTemplate = useCallback((template: ConnectionTemplate) => {
    setTemplates((prev) => {
      const updated = [...prev.filter((t) => t.id !== template.id), { ...template, isCustom: true }];
      localStorage.setItem("ssh-custom-templates", JSON.stringify(updated));
      return updated;
    });
  }, []);

  const deleteTemplate = useCallback((id: string) => {
    setTemplates((prev) => {
      const updated = prev.filter((t) => t.id !== id);
      localStorage.setItem("ssh-custom-templates", JSON.stringify(updated));
      return updated;
    });
  }, []);

  return { templates, saveTemplate, deleteTemplate };
}
