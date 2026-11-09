import { useEffect, useState } from "react";
import type { HostKeyRequest } from "../../../electron/ssh-known-hosts";
import { HostKeyDialog } from "./HostKeyDialog";
import { publishToast } from "../../lib/toasts";

interface HostKeyBridge {
  answerHostKey(requestId: string, accepted: boolean): Promise<void>;
  onHostKeyRequest(listener: (request: HostKeyRequest) => void): () => void;
  onHostKeyClosed(listener: (payload: { requestId: string }) => void): () => void;
}

export function HostKeyPrompts() {
  const [requests, setRequests] = useState<HostKeyRequest[]>([]);
  const bridge = (window as unknown as { crab?: { ssh?: HostKeyBridge } }).crab?.ssh;
  useEffect(() => {
    if (!bridge) return;
    const stopRequest = bridge.onHostKeyRequest(request => setRequests(current => [...current, request]));
    const stopClosed = bridge.onHostKeyClosed(({ requestId }) => setRequests(current => current.filter(request => request.requestId !== requestId)));
    return () => { stopRequest(); stopClosed(); };
  }, [bridge]);
  const request = requests[0];
  if (!request || !bridge) return null;
  const answer = (accepted: boolean) => {
    void bridge.answerHostKey(request.requestId, accepted).catch(error => {
      publishToast({ title: "SSH host verification failed", body: error instanceof Error ? error.message : String(error), kind: "error" });
    });
  };
  return <HostKeyDialog key={request.requestId} {...request} open onAccept={() => answer(true)} onReject={() => answer(false)} />;
}
