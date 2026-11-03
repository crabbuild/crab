/**
 * Transfer Security Display — shows cipher algorithm, MAC algorithm,
 * KEX method, and warning dialog for weak/deprecated ciphers.
 */

import { useState, useCallback } from "react";
import {
  Shield,
  ShieldAlert,
  ShieldCheck,
  Lock,
  Key,
  AlertTriangle,
} from "lucide-react";
import clsx from "clsx";

// ── Types ────────────────────────────────────────────────────────

interface TransferSecurityInfo {
  cipher: string;
  mac: string;
  kex: string;
}

interface TransferSecurityDisplayProps {
  security: TransferSecurityInfo | null;
  /** Whether the current cipher is considered weak/deprecated */
  isWeak?: boolean;
  /** Description of why the cipher is weak */
  weakReason?: string;
  /** Callback when user confirms proceeding with weak cipher */
  onConfirmWeak?: () => void;
  /** Callback when user cancels due to weak cipher */
  onCancelWeak?: () => void;
}

// ── Constants ────────────────────────────────────────────────────

const DEPRECATED_CIPHERS = new Set([
  "des",
  "3des",
  "3des-cbc",
  "des-cbc",
  "rc4",
  "arcfour",
  "arcfour128",
  "arcfour256",
  "blowfish-cbc",
  "cast128-cbc",
]);

function isCipherDeprecated(cipher: string): boolean {
  return DEPRECATED_CIPHERS.has(cipher.toLowerCase());
}

// ── Weak Cipher Warning Dialog ───────────────────────────────────

function WeakCipherDialog({
  cipher,
  reason,
  onConfirm,
  onCancel,
}: {
  cipher: string;
  reason: string;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50" role="dialog" aria-modal="true" aria-label="Weak cipher warning">
      <div className="bg-white dark:bg-gray-800 rounded-lg shadow-xl max-w-md w-full mx-4 p-4 space-y-3">
        <div className="flex items-start gap-3">
          <ShieldAlert className="w-6 h-6 text-red-500 shrink-0 mt-0.5" />
          <div>
            <h3 className="text-sm font-semibold text-gray-800 dark:text-gray-200">
              Weak Cipher Detected
            </h3>
            <p className="text-xs text-gray-600 dark:text-gray-400 mt-1">
              The SSH connection is using a cipher that may not provide adequate security.
            </p>
          </div>
        </div>

        <div className="bg-red-50 dark:bg-red-900/20 border border-red-200 dark:border-red-800 rounded-md p-2 space-y-1">
          <p className="text-xs font-mono text-red-700 dark:text-red-300">
            Cipher: {cipher}
          </p>
          <p className="text-xs text-red-600 dark:text-red-400">
            {reason}
          </p>
        </div>

        <p className="text-xs text-gray-600 dark:text-gray-400">
          Proceeding with this cipher may expose your data to interception. It is recommended
          to cancel and configure a stronger cipher in your SSH settings.
        </p>

        <div className="flex justify-end gap-2 pt-1">
          <button
            onClick={onCancel}
            className="px-4 py-1.5 text-xs bg-gray-100 dark:bg-gray-700 text-gray-700 dark:text-gray-300 rounded hover:bg-gray-200 dark:hover:bg-gray-600"
          >
            Cancel Transfer
          </button>
          <button
            onClick={onConfirm}
            className="px-4 py-1.5 text-xs bg-red-600 text-white rounded hover:bg-red-700"
          >
            Proceed Anyway
          </button>
        </div>
      </div>
    </div>
  );
}

// ── Main Component ───────────────────────────────────────────────

export function TransferSecurityDisplay({
  security,
  isWeak = false,
  weakReason,
  onConfirmWeak,
  onCancelWeak,
}: TransferSecurityDisplayProps) {
  const [showWarning, setShowWarning] = useState(isWeak);

  const handleConfirm = useCallback(() => {
    setShowWarning(false);
    onConfirmWeak?.();
  }, [onConfirmWeak]);

  const handleCancel = useCallback(() => {
    setShowWarning(false);
    onCancelWeak?.();
  }, [onCancelWeak]);

  if (!security) {
    return (
      <div className="flex items-center gap-2 px-3 py-2 text-xs text-gray-500 dark:text-gray-400">
        <Shield className="w-4 h-4" />
        <span>Security information unavailable</span>
      </div>
    );
  }

  const deprecated = isCipherDeprecated(security.cipher);
  const StatusIcon = deprecated || isWeak ? ShieldAlert : ShieldCheck;
  const statusColor = deprecated || isWeak
    ? "text-red-500"
    : "text-green-500";

  return (
    <>
      <div className="space-y-2 px-3 py-2" role="region" aria-label="Transfer security details">
        {/* Status header */}
        <div className="flex items-center gap-2">
          <StatusIcon className={clsx("w-4 h-4", statusColor)} />
          <span className={clsx("text-xs font-medium", deprecated || isWeak ? "text-red-600 dark:text-red-400" : "text-green-600 dark:text-green-400")}>
            {deprecated || isWeak ? "Weak Encryption" : "Secure Connection"}
          </span>
        </div>

        {/* Security details */}
        <div className="space-y-1.5 pl-6">
          <div className="flex items-center gap-2">
            <Lock className="w-3 h-3 text-gray-400 dark:text-gray-500" />
            <span className="text-[10px] text-gray-500 dark:text-gray-400 w-12">Cipher</span>
            <span
              className={clsx(
                "text-xs font-mono",
                deprecated
                  ? "text-red-600 dark:text-red-400"
                  : "text-gray-700 dark:text-gray-300",
              )}
            >
              {security.cipher}
              {deprecated && (
                <span className="ml-1.5 text-[10px] text-red-500 font-sans">(deprecated)</span>
              )}
            </span>
          </div>
          <div className="flex items-center gap-2">
            <Shield className="w-3 h-3 text-gray-400 dark:text-gray-500" />
            <span className="text-[10px] text-gray-500 dark:text-gray-400 w-12">MAC</span>
            <span className="text-xs font-mono text-gray-700 dark:text-gray-300">
              {security.mac}
            </span>
          </div>
          <div className="flex items-center gap-2">
            <Key className="w-3 h-3 text-gray-400 dark:text-gray-500" />
            <span className="text-[10px] text-gray-500 dark:text-gray-400 w-12">KEX</span>
            <span className="text-xs font-mono text-gray-700 dark:text-gray-300">
              {security.kex}
            </span>
          </div>
        </div>

        {/* Inline warning for deprecated ciphers */}
        {(deprecated || isWeak) && !showWarning && (
          <div className="flex items-start gap-2 mt-2 p-2 bg-red-50 dark:bg-red-900/10 border border-red-200 dark:border-red-800 rounded">
            <AlertTriangle className="w-3.5 h-3.5 text-red-500 shrink-0 mt-0.5" />
            <p className="text-[10px] text-red-600 dark:text-red-400">
              {weakReason || "This cipher is deprecated and may not provide adequate security for file transfers."}
            </p>
          </div>
        )}
      </div>

      {/* Warning dialog */}
      {showWarning && onConfirmWeak && onCancelWeak && (
        <WeakCipherDialog
          cipher={security.cipher}
          reason={weakReason || "This cipher is deprecated and does not meet minimum security requirements."}
          onConfirm={handleConfirm}
          onCancel={handleCancel}
        />
      )}
    </>
  );
}

// ── Exports for testing ──────────────────────────────────────────

export { isCipherDeprecated, DEPRECATED_CIPHERS };
