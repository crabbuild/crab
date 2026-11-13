import { useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { Plus } from "lucide-react";
import { Button } from "../../src/components/ui/button";
import { Input } from "../../src/components/ui/input";
import { Textarea } from "../../src/components/ui/textarea";
import {
  Select,
  SelectTrigger,
  SelectValue,
  SelectContent,
  SelectGroup,
  SelectItem,
} from "../../src/components/ui/select";
import { Tabs, TabsContent } from "../../src/components/ui/tabs";
import {
  WorkflowTabs,
  type WorkflowTab,
} from "../../src/components/workflow/WorkflowTabs";
import { ConfirmHydrateDialog } from "../../src/components/ConfirmHydrateDialog";
import { ToastManager } from "../../src/components/ToastManager";
import { publishToast } from "../../src/lib/toasts";
import { StoreProvider } from "../../src/state";
import {
  SettingsProvider,
  useSettings,
  type Settings,
} from "../../src/settings";
import "../../src/index.css";

// Only the main-process settings mirror is stubbed in this browser fixture;
// the production providers, persistence, controls, and dialogs run unchanged.
Object.assign(window, {
  crab: {
    updateMainSettings: async () => ({}),
    setDevtools: async () => {},
    onEvent: () => () => {},
    rpc: async () => ({ version: 0, jobs: [], error: null }),
  },
});

function Fixture() {
  const { settings, update } = useSettings();
  const [tab, setTab] = useState<WorkflowTab>("editor");
  const [dialog, setDialog] = useState(false);
  const [result, setResult] = useState("");
  const input = useRef<HTMLInputElement>(null);
  return (
    <main className="mx-auto max-w-5xl space-y-6 p-6">
      <header className="flex items-center justify-between border-b border-border pb-4">
        <h1 className="text-lg font-semibold">Crab desktop theme</h1>
        <Button onClick={() => publishToast({ kind: "success", title: "Theme notification", body: "Changes saved successfully.", duration: 0 })}>Show notification</Button>
        <div className="flex items-center gap-3 text-muted-foreground">
          <label htmlFor="theme">Theme</label>
          <Select
            value={settings.theme}
            onValueChange={(value) =>
              update("theme", value as Settings["theme"])
            }
          >
            <SelectTrigger id="theme" aria-label="Theme">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectGroup>
                {(["light", "dark", "hc-light", "hc-dark", "system"] as const).map(
              (mode) => (
                <SelectItem key={mode} value={mode}>
                    {mode}
                  </SelectItem>
                ))}
              </SelectGroup>
            </SelectContent>
          </Select>
        </div>
      </header>
      <section
        aria-label="Workflow studio"
        className="overflow-hidden rounded-lg border border-border bg-card"
      >
        <Tabs
          value={tab}
          onValueChange={(value) => setTab(value as WorkflowTab)}
        >
          <div className="flex h-9 items-center justify-between border-b border-border pr-2">
            <WorkflowTabs outdatedCount={2} />
            <span className="text-xs text-muted-foreground">crab.yaml</span>
          </div>
          <TabsContent value="editor" className="m-0 flex flex-col p-4">
            <form
              className="space-y-4"
              onSubmit={(event) => {
                event.preventDefault();
                const fields = new FormData(event.currentTarget);
                setResult(
                  `${fields.get("stage")}: ${fields.get("command")} (${fields.get("runner")})`,
                );
              }}
            >
              <div className="space-y-1">
                <label htmlFor="stage" className="block">
                  Stage name
                </label>
                <Input
                  id="stage"
                  ref={input}
                  required
                  name="stage"
                  defaultValue="example"
                  className="block w-full"
                />
              </div>
              <div className="space-y-1">
                <label htmlFor="command" className="block">
                  Command
                </label>
                <Textarea
                  id="command"
                  name="command"
                  defaultValue="echo hello"
                  className="block w-full font-mono"
                />
              </div>
              <div className="space-y-1">
                <label htmlFor="runner" className="block">
                  Runner
                </label>
                <Select name="runner" defaultValue="Shell">
                  <SelectTrigger id="runner" aria-label="Runner">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectGroup>
                      <SelectItem value="Shell">Shell</SelectItem>
                      <SelectItem value="Python">Python</SelectItem>
                    </SelectGroup>
                  </SelectContent>
                </Select>
              </div>
              <div className="flex flex-wrap items-center gap-2">
                <Button type="submit" variant="default">
                  Save stage
                </Button>
                <Button type="button" onClick={() => input.current?.focus()}>
                  Focus stage
                </Button>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon"
                  aria-label="Add stage"
                >
                  <Plus size={14} />
                </Button>
                <Button
                  type="button"
                  variant="destructive"
                  onClick={() => setDialog(true)}
                >
                  Review disk changes
                </Button>
                <Button disabled>Unavailable</Button>
              </div>
            </form>
          </TabsContent>
          <TabsContent value="status" className="m-0 p-4">
            Workflow status
          </TabsContent>
          <TabsContent value="experiments" className="m-0 p-4">
            Workflow experiments
          </TabsContent>
        </Tabs>
      </section>
      <section aria-label="Run status" className="flex flex-wrap gap-3 text-xs">
        {(
          ["success", "warning", "destructive", "info", "thinking"] as const
        ).map((status) => (
          <span
            key={status}
            style={{
              color: `rgb(var(--${status}))`,
              background: `rgb(var(--${status}) / .1)`,
            }}
            className="rounded border border-border px-2 py-1"
          >
            {status}
          </span>
        ))}
      </section>
      <output
        className="block rounded border border-border bg-muted p-3 font-mono"
        aria-live="polite"
      >
        {result || "No changes saved"}
      </output>
      <ConfirmHydrateDialog
        open={dialog}
        verb="hydrate"
        fileCount={2}
        totalBytes={2048}
        freeBytes={1024}
        busy={false}
        onCancel={() => setDialog(false)}
        onConfirm={() => {
          setDialog(false);
          setResult("Disk change confirmed");
        }}
      />
      <ToastManager />
    </main>
  );
}
createRoot(document.getElementById("root")!).render(
  <StoreProvider>
    <SettingsProvider>
      <Fixture />
    </SettingsProvider>
  </StoreProvider>,
);
