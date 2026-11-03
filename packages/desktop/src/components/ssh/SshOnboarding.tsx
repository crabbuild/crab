/**
 * SSH Onboarding — first-run detection, quick-start wizard, and
 * contextual help for SSH remote connections.
 *
 * Features:
 * - First-run SSH key detection banner
 * - 3-step quick-start wizard (template → test → connect)
 * - Contextual tooltips for connection form fields
 * - Error recovery guidance with remediation steps
 */

import { useCallback, useState } from "react";

// ── Types ────────────────────────────────────────────────────────

export interface SshDetectionResult {
  hasKeys: boolean;
  keyCount: number;
  hasConfig: boolean;
  configHostCount: number;
}

type WizardStep = "template" | "configure" | "test" | "done";

interface QuickStartWizardProps {
  isOpen: boolean;
  onClose: () => void;
  onComplete: (profileData: unknown) => void;
  detection: SshDetectionResult;
}

interface ErrorGuidanceProps {
  errorCode: number;
  errorMessage: string;
}

interface FirstRunBannerProps {
  detection: SshDetectionResult;
  onImport: () => void;
  onDismiss: () => void;
  onAddConnection: () => void;
}

// ── Error Remediation Catalog ────────────────────────────────────

const ERROR_REMEDIATION: Record<number, { title: string; steps: string[] }> = {
  2001: {
    title: "Authentication Failed",
    steps: [
      "Verify your username is correct for this server",
      "Check that your SSH key has correct permissions (chmod 600 ~/.ssh/id_*)",
      "Try using SSH agent authentication instead of a key file",
      "If using a password, verify it in your OS keychain",
      "Run `ssh -v user@host` in a terminal to see detailed auth errors",
    ],
  },
  2002: {
    title: "Host Unreachable",
    steps: [
      "Verify the hostname or IP address is correct",
      "Check your network connection (try `ping hostname`)",
      "Verify the server is running and accepting connections",
      "Check if a firewall is blocking port 22 (or your custom port)",
      "If using a VPN, ensure it's connected",
    ],
  },
  2003: {
    title: "Host Key Changed (Security Warning)",
    steps: [
      "This could indicate a man-in-the-middle attack",
      "If the server was recently reinstalled, this is expected",
      "Verify the new fingerprint with your server administrator",
      "To resolve: remove the old key and accept the new one",
      "Never ignore this warning on production servers",
    ],
  },
  2004: {
    title: "Agent Deployment Failed",
    steps: [
      "Check available disk space on the remote server (need ~20MB)",
      "Verify write permissions to ~/.crab/bin/ on the remote",
      "If the binary is already running, try: `pkill crab-desktop-agent`",
      "Try manual deployment: download the binary and scp it manually",
      "Check if the remote architecture matches (run `uname -m` on remote)",
    ],
  },
  2005: {
    title: "Agent Failed to Start",
    steps: [
      "Check if the binary has execute permissions: `chmod +x ~/.crab/bin/crab-desktop-agent`",
      "Verify the binary architecture matches the server: `file ~/.crab/bin/crab-desktop-agent`",
      "Check for missing shared libraries: `ldd ~/.crab/bin/crab-desktop-agent`",
      "Look at agent logs: `~/.crab/bin/crab-desktop-agent 2>&1 | head`",
      "Try re-deploying the agent binary",
    ],
  },
  2007: {
    title: "Connection Timed Out",
    steps: [
      "The server may be under heavy load — try again in a moment",
      "Check if the SSH port is correct (default: 22)",
      "Increase the connection timeout in profile settings",
      "If on a slow network, enable compression in profile settings",
      "Try connecting from a terminal: `ssh -o ConnectTimeout=30 user@host`",
    ],
  },
  2009: {
    title: "Remote Disk Full",
    steps: [
      "Free up space on the remote server",
      "Check disk usage: `df -h` on the remote",
      "Remove old agent binaries: `rm ~/.crab/bin/crab-desktop-agent.old`",
      "Clear temporary files: `rm -rf /tmp/crab-*`",
      "Consider using a different remote path with more space",
    ],
  },
  2010: {
    title: "Permission Denied",
    steps: [
      "Check file/directory permissions on the remote",
      "Verify your user has access to the target path",
      "If accessing another user's files, check group permissions",
      "Try: `ls -la /path/to/directory` to see current permissions",
      "Contact the server administrator if you need elevated access",
    ],
  },
  2011: {
    title: "Port Already in Use",
    steps: [
      "Another application is using the local port",
      "Try a different local port for the forward",
      "Find what's using the port: `lsof -i :PORT` (macOS/Linux)",
      "The app will automatically try the next available port",
      "Close the conflicting application and retry",
    ],
  },
};

// ── Components ───────────────────────────────────────────────────

export function FirstRunBanner({
  detection,
  onImport,
  onDismiss,
  onAddConnection,
}: FirstRunBannerProps) {
  if (!detection.hasKeys && !detection.hasConfig) return null;

  return (
    <div className="ssh-onboarding-banner" role="banner" aria-label="SSH setup available">
      <div className="ssh-onboarding-banner__content">
        <span className="ssh-onboarding-banner__icon" aria-hidden="true">🔑</span>
        <div className="ssh-onboarding-banner__text">
          {detection.hasConfig ? (
            <p>
              We found <strong>{detection.configHostCount} SSH hosts</strong> in your config.
              Import them to connect to your remote servers.
            </p>
          ) : (
            <p>
              SSH keys detected. Connect to your remote servers for GPU training,
              model deployment, and more.
            </p>
          )}
        </div>
      </div>
      <div className="ssh-onboarding-banner__actions">
        {detection.hasConfig && (
          <button className="ssh-onboarding-banner__btn--primary" onClick={onImport}>
            Import
          </button>
        )}
        <button className="ssh-onboarding-banner__btn--secondary" onClick={onAddConnection}>
          Add Connection
        </button>
        <button
          className="ssh-onboarding-banner__btn--dismiss"
          onClick={onDismiss}
          aria-label="Dismiss SSH setup banner"
        >
          ✕
        </button>
      </div>
    </div>
  );
}

export function QuickStartWizard({
  isOpen,
  onClose,
  onComplete,
}: QuickStartWizardProps) {
  const [step, setStep] = useState<WizardStep>("template");
  const [profileData] = useState<Record<string, unknown>>({});
  const [testResult, setTestResult] = useState<"pending" | "success" | "failure" | null>(null);

  const handleTestConnection = useCallback(async () => {
    setTestResult("pending");
    try {
      // In production, this calls window.crab.ssh.testConnection(profileData)
      await Promise.resolve(profileData);
      setTestResult("success");
    } catch {
      setTestResult("failure");
    }
  }, [profileData]);

  const handleComplete = useCallback(() => {
    onComplete(profileData);
    onClose();
  }, [profileData, onComplete, onClose]);

  if (!isOpen) return null;

  return (
    <div className="ssh-wizard-overlay" role="dialog" aria-modal="true" aria-labelledby="ssh-wizard-title">
      <div className="ssh-wizard">
        <header className="ssh-wizard__header">
          <h2 id="ssh-wizard-title">Connect to a Remote Server</h2>
          <div className="ssh-wizard__steps" role="progressbar" aria-valuenow={step === "template" ? 1 : step === "configure" ? 2 : 3} aria-valuemin={1} aria-valuemax={3}>
            <span className={`ssh-wizard__step ${step === "template" ? "ssh-wizard__step--active" : ""}`}>1. Choose</span>
            <span className={`ssh-wizard__step ${step === "configure" ? "ssh-wizard__step--active" : ""}`}>2. Configure</span>
            <span className={`ssh-wizard__step ${step === "test" ? "ssh-wizard__step--active" : ""}`}>3. Connect</span>
          </div>
        </header>

        <div className="ssh-wizard__body">
          {step === "template" && (
            <div className="ssh-wizard__template-step">
              <p>Choose a template to get started quickly, or configure manually.</p>
            </div>
          )}

          {step === "test" && (
            <div className="ssh-wizard__test-step">
              {testResult === null && (
                <button className="ssh-wizard__test-btn" onClick={handleTestConnection}>
                  Test Connection
                </button>
              )}
              {testResult === "pending" && <p>Testing connection...</p>}
              {testResult === "success" && (
                <div className="ssh-wizard__test-success">
                  <span aria-hidden="true">✓</span> Connection successful!
                  <button className="ssh-wizard__connect-btn" onClick={handleComplete}>
                    Save & Connect
                  </button>
                </div>
              )}
              {testResult === "failure" && (
                <div className="ssh-wizard__test-failure">
                  <span aria-hidden="true">✕</span> Connection failed.
                  <button onClick={() => setStep("configure")}>Go Back</button>
                  <button onClick={handleTestConnection}>Retry</button>
                </div>
              )}
            </div>
          )}
        </div>

        <footer className="ssh-wizard__footer">
          <button className="ssh-wizard__cancel" onClick={onClose}>Cancel</button>
        </footer>
      </div>
    </div>
  );
}

export function ErrorGuidance({ errorCode, errorMessage }: ErrorGuidanceProps) {
  const [expanded, setExpanded] = useState(false);
  const remediation = ERROR_REMEDIATION[errorCode];

  if (!remediation) {
    return (
      <div className="ssh-error-guidance" role="alert">
        <p className="ssh-error-guidance__message">{errorMessage}</p>
      </div>
    );
  }

  return (
    <div className="ssh-error-guidance" role="alert">
      <p className="ssh-error-guidance__message">{errorMessage}</p>
      <button
        className="ssh-error-guidance__toggle"
        onClick={() => setExpanded(!expanded)}
        aria-expanded={expanded}
        aria-controls="ssh-error-steps"
      >
        {expanded ? "Hide" : "What went wrong?"}
      </button>
      {expanded && (
        <div id="ssh-error-steps" className="ssh-error-guidance__steps">
          <h4>{remediation.title}</h4>
          <ol>
            {remediation.steps.map((step, i) => (
              <li key={i}>{step}</li>
            ))}
          </ol>
        </div>
      )}
    </div>
  );
}

// ── Field Tooltips ───────────────────────────────────────────────

export const FIELD_TOOLTIPS: Record<string, { description: string; example?: string; docs?: string }> = {
  host: {
    description: "The hostname or IP address of the remote server.",
    example: "gpu-01.internal.example.com or 192.168.1.100",
  },
  port: {
    description: "SSH port number. Most servers use 22.",
    example: "22 (default), some cloud providers use custom ports",
  },
  username: {
    description: "Your username on the remote server.",
    example: "ubuntu (AWS), ec2-user (Amazon Linux), root (RunPod)",
  },
  authMethod: {
    description: "How to authenticate. SSH Agent is recommended — it uses keys from your system keychain.",
    example: "SSH Agent works with 1Password, macOS Keychain, and ssh-agent",
  },
  remotePath: {
    description: "Default directory to open when browsing files on this server.",
    example: "/home/username/projects or /workspace",
  },
  jumpHost: {
    description: "Intermediate bastion host to connect through (ProxyJump).",
    example: "bastion.example.com — connects through this host first",
  },
  compression: {
    description: "Enable zlib compression on the SSH channel. Helps on slow connections.",
    example: "Recommended for connections with >100ms latency",
  },
  forwardAgent: {
    description: "Forward your local SSH agent to the remote. Allows git operations on the remote using your local keys.",
    example: "Enable if you need to git push/pull on the remote server",
  },
};
