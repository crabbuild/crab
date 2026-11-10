import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import type { SshTerminalRequest } from "../../../src/views/TerminalView";
import { TerminalKeepAlive } from "../../../src/components/TerminalKeepAlive";
import { SettingsProvider } from "../../../src/settings";
import { StoreProvider, type OpenRepo } from "../../../src/state";
import { PtyModeProvider } from "../../../src/lib/pty-mode-context";
import "../../../src/index.css";

function Fixture() {
  const root = new URLSearchParams(location.search).get("root")!;
  const [workspace, setWorkspace] = useState("a");
  const [generation, setGeneration] = useState(0);
  const [repo, setRepo] = useState("one");
  const [cap, setCap] = useState(2);
  const [sshRequest, setSshRequest] = useState<SshTerminalRequest | null>(null);
  const repos = ["one", "two"].map(name => ({
    repo_id: `${workspace}-${name}-${generation}`, path: `${root}/${workspace}/${name}`,
    label: `${workspace}/${name}`, url: "", deferred: true,
  } as OpenRepo));
  return <div style={{ height: "100vh", display: "flex", flexDirection: "column" }}>
    <div style={{ display: "flex", gap: 16, padding: 8 }}>
      <button onClick={() => setRepo(repo === "one" ? "two" : "one")}>Switch repository</button>
      <button onClick={() => { setWorkspace(workspace === "a" ? "b" : "a"); setGeneration(value => value + 1); }}>Switch workspace</button>
      <button onClick={() => setCap(cap === 1 ? 2 : 1)}>Keep {cap} renderers</button>
      <button onClick={() => setSshRequest({ profileId: "fixture", hostname: "loopback", cwd: `${root}/remote` })}>Open SSH terminal</button>
      <span>Active: {workspace}/{repo}</span>
    </div>
    <TerminalKeepAlive sshRequest={sshRequest} onSshRequestHandled={() => setSshRequest(null)} repos={repos} activeRepoId={`${workspace}-${repo}-${generation}`} maxKeptAlive={cap} />
  </div>;
}
createRoot(document.getElementById("root")!).render(<SettingsProvider><StoreProvider><PtyModeProvider><Fixture /></PtyModeProvider></StoreProvider></SettingsProvider>);
