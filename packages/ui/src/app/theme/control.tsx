import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
} from "@/components/ui/dropdown-menu";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import {
  Tooltip,
  TooltipTrigger,
  TooltipContent,
} from "@/components/ui/tooltip";
import { MonitorIcon, SunIcon, MoonIcon, PaintbrushIcon } from "lucide-react";
import { codeThemeChoices, codeThemeFrom, type CodeTheme } from "@/code-theme";

export type Theme = "light" | "dark" | "auto";

const choices = [
  { value: "auto", label: "System", icon: MonitorIcon },
  { value: "light", label: "Light", icon: SunIcon },
  { value: "dark", label: "Dark", icon: MoonIcon },
] as const;

export function ThemeControl({
  theme,
  setTheme,
  codeTheme,
  setCodeTheme,
}: {
  theme: Theme;
  setTheme: (theme: Theme) => void;
  codeTheme: CodeTheme;
  setCodeTheme: (theme: CodeTheme) => void;
}) {
  const selected = codeThemeChoices[codeTheme];
  return (
    <div className="theme-control">
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button
            variant="outline"
            size="icon-sm"
            aria-label={`Code theme: ${selected.label}`}
            title={`Code theme: ${selected.label}`}
          >
            <PaintbrushIcon data-icon="inline-start" />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-64">
          <DropdownMenuRadioGroup
            value={codeTheme}
            onValueChange={(value) => setCodeTheme(codeThemeFrom(value))}
          >
            {Object.entries(codeThemeChoices).map(([value, choice]) => (
              <DropdownMenuRadioItem value={value} key={value}>
                <div>
                  <div>{choice.label}</div>
                  <p className="text-xs text-muted-foreground">
                    {choice.description}
                  </p>
                </div>
              </DropdownMenuRadioItem>
            ))}
          </DropdownMenuRadioGroup>
        </DropdownMenuContent>
      </DropdownMenu>
      <ToggleGroup
        role="group"
        type="single"
        variant="outline"
        size="sm"
        spacing={0}
        aria-label="Appearance"
        value={theme}
        onValueChange={(value) => {
          if (value === "auto" || value === "light" || value === "dark")
            setTheme(value);
        }}
      >
        {choices.map(({ value, label, icon: Icon }) => (
          <Tooltip key={value}>
            <TooltipTrigger asChild>
              <ToggleGroupItem
                value={value}
                aria-label={label}
                role="button"
                aria-pressed={theme === value}
                aria-checked={undefined}
              >
                <Icon data-icon="inline-start" />
              </ToggleGroupItem>
            </TooltipTrigger>
            <TooltipContent>
              Use {label.toLowerCase()} appearance
            </TooltipContent>
          </Tooltip>
        ))}
      </ToggleGroup>
    </div>
  );
}
