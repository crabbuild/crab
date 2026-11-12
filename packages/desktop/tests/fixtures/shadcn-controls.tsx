import { PasteConfirmDialog } from "../../src/components/PasteConfirmDialog";
import type { PasteAnalysis } from "../../src/lib/terminal-paste-guard";
import { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { Button } from "../../src/components/ui/button";
import { Input } from "../../src/components/ui/input";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../src/components/ui/select";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuPortal,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "../../src/components/ui/dropdown-menu";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "../../src/components/ui/popover";
import { Checkbox } from "../../src/components/ui/checkbox";
import { Switch } from "../../src/components/ui/switch";
import { Slider } from "../../src/components/ui/slider";
import {
  ToggleGroup,
  ToggleGroupItem,
} from "../../src/components/ui/toggle-group";
import {
  Command,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "../../src/components/ui/command";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "../../src/components/ui/collapsible";
import { ApplicationDialogs } from "../../src/components/ApplicationDialogs";
import { TerminalPresetMenu } from "../../src/components/PresetDropdown";
import {
  requestConfirmation,
  requestPrompt,
} from "../../src/lib/application-dialogs";
import { ToastManager } from "../../src/components/ToastManager";
import { publishToast } from "../../src/lib/toasts";
import "../../src/index.css";

const pasteCases: Record<string, PasteAnalysis> = {
  multi: { requiresConfirmation: true, reasons: ["multi-line"], lineCount: 42, controlChars: [], dangerousPatterns: [], preview: "first\n...\nlast" },
  control: { requiresConfirmation: true, reasons: ["control-chars"], lineCount: 1, controlChars: ["^C", "^["], dangerousPatterns: [], preview: "hello^Cworld^[end" },
  dangerous: { requiresConfirmation: true, reasons: ["control-chars", "multi-line", "dangerous-pattern"], lineCount: 10, controlChars: ["^C"], dangerousPatterns: ["rm\\s+-rf\\s+/"], preview: "rm -rf /^C\n2\n...\n10" },
};

function Fixture() {
  const [value, setValue] = useState("one");
  const [pasteCase, setPasteCase] = useState<string | null>(null);
  const [result, setResult] = useState("Ready");
  const [checked, setChecked] = useState<boolean | "indeterminate">(
    "indeterminate",
  );
  const [enabled, setEnabled] = useState(false);
  const [mode, setMode] = useState("list");
  const [slider, setSlider] = useState([12]);
  const [shortcut, setShortcut] = useState(0);
  useEffect(() => {
    const handleKey = (event: KeyboardEvent) => {
      if (event.key === "n") setShortcut((count) => count + 1);
    };
    window.addEventListener("keydown", handleKey);
    return () => window.removeEventListener("keydown", handleKey);
  }, []);
  return (
    <main className="mx-auto grid max-w-3xl gap-5 p-8">
      <h1 className="text-lg font-semibold">Crab shadcn controls</h1>
      <Select
        value={":" + value}
        onValueChange={(selected) => {
          setValue(selected.slice(1));
          setResult(`Selection: ${selected.slice(1) || "None"}`);
        }}
      >
        <SelectTrigger aria-label="Selection">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectGroup>
            <SelectItem value=":">None</SelectItem>
            <SelectItem value=":one">One</SelectItem>
            <SelectItem value=":next">Next</SelectItem>
            <SelectItem value=":disabled" disabled>
              Disabled
            </SelectItem>
          </SelectGroup>
        </SelectContent>
      </Select>
      <div className="flex flex-wrap gap-3">
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button>Actions</Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent>
            <DropdownMenuGroup>
              <DropdownMenuItem disabled>Unavailable action</DropdownMenuItem>
              <DropdownMenuSub>
                <DropdownMenuSubTrigger>Export</DropdownMenuSubTrigger>
                <DropdownMenuPortal>
                  <DropdownMenuSubContent>
                    <DropdownMenuGroup>
                      <DropdownMenuItem
                        onSelect={() => setResult("Exported text")}
                      >
                        Text
                      </DropdownMenuItem>
                    </DropdownMenuGroup>
                  </DropdownMenuSubContent>
                </DropdownMenuPortal>
              </DropdownMenuSub>
            </DropdownMenuGroup>
          </DropdownMenuContent>
        </DropdownMenu>
        <Popover>
          <PopoverTrigger asChild>
            <Button>Edit settings</Button>
          </PopoverTrigger>
          <PopoverContent>
            <label className="grid gap-2">
              Setting name
              <Input aria-label="Setting name" defaultValue="example" />
            </label>
            <Button
              className="mt-3"
              onClick={() => setResult("Settings applied")}
            >
              Apply settings
            </Button>
          </PopoverContent>
        </Popover>
        <Button
          onClick={async () =>
            setResult(
              `Prompt: ${(await requestPrompt("Name", "example")) ?? "cancelled"}`,
            )
          }
        >
          Prompt
        </Button>
        <Button
          onClick={async () => {
            const first = requestConfirmation("Delete entry?");
            const second = requestPrompt("Queued name", "queued");
            setResult(`Queue: ${await first}, ${await second}`);
          }}
        >
          Queue dialogs
        </Button>
        <TerminalPresetMenu
          onSelect={(command) => setResult(`Launch: ${command.command}`)}
          onSpawnPlain={() => setResult("Plain terminal")}
        >
          <Button>Terminal presets</Button>
        </TerminalPresetMenu>
        <Button
          onClick={() =>
            publishToast({
              kind: "success",
              title: "Saved",
              duration: 0,
              action: {
                label: "Undo",
                onClick: () => setResult("Undo clicked"),
              },
            })
          }
        >
          Notify
        </Button>
      </div>
      <div className="flex items-center gap-6">
        <label className="flex items-center gap-2">
          <Checkbox checked={checked} onCheckedChange={setChecked} />
          Include files
        </label>
        <label className="flex items-center gap-2">
          <Switch checked={enabled} onCheckedChange={setEnabled} />
          Enable feature
        </label>
        <ToggleGroup
          type="single"
          value={mode}
          onValueChange={(value) => {
            if (value) setMode(value);
          }}
          aria-label="View mode"
        >
          <ToggleGroupItem value="list">List</ToggleGroupItem>
          <ToggleGroupItem value="tree">Tree</ToggleGroupItem>
        </ToggleGroup>
      </div>
      <div>
        <label id="font-size">Font size: {slider[0]}</label>
        <Slider
          aria-labelledby="font-size"
          value={slider}
          min={8}
          max={32}
          step={1}
          onValueChange={setSlider}
        />
      </div>
      <Command className="border" aria-label="Find command">
        <CommandInput placeholder="Find command" />
        <CommandList>
          <CommandGroup>
            <CommandItem onSelect={() => setResult("Command alpha")}>
              Alpha
            </CommandItem>
            <CommandItem onSelect={() => setResult("Command beta")}>
              Beta
            </CommandItem>
          </CommandGroup>
        </CommandList>
      </Command>
      <Collapsible>
        <CollapsibleTrigger>Advanced settings</CollapsibleTrigger>
        <CollapsibleContent>Advanced content</CollapsibleContent>
      </Collapsible>
      <div className="flex gap-2">{Object.keys(pasteCases).map(kind => <Button key={kind} onClick={() => setPasteCase(kind)}>Review {kind} paste</Button>)}</div>
      {pasteCase && <PasteConfirmDialog analysis={pasteCases[pasteCase]} onCancel={() => { setPasteCase(null); setResult("Paste cancelled"); }} onConfirm={() => { setPasteCase(null); setResult("Paste confirmed"); }} />}
      <output
        data-testid="result"
        className="rounded border p-3"
        aria-live="polite"
      >
        {result}
      </output>
      <p data-testid="shortcut">Workspace shortcut: {shortcut}</p>
      <p data-testid="state">
        {String(checked)} · {String(enabled)} · {mode}
      </p>
      <ApplicationDialogs />
      <ToastManager />
    </main>
  );
}
createRoot(document.getElementById("root")!).render(<Fixture />);
