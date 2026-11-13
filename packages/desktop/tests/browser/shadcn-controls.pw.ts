import { expect, test } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

test.beforeEach(async ({ page }) => {
  await page.goto("/tests/fixtures/shadcn-controls.html");
});

for (const mode of ["Daemon", "Normal"] as const) {
  test(`startup readiness restores ${mode} mode after an early IPC query fails`, async ({ page }) => {
    await page.goto("/tests/fixtures/shadcn-controls.html?startup-mode");
    await expect(page.getByTestId("startup-mode")).toHaveText("false:false:in-process:auto");
    await page.getByRole("button", { name: `${mode} ready`, exact: true }).click();
    await expect(page.getByTestId("startup-mode")).toHaveText(mode === "Daemon"
      ? "false:true:daemon:daemon" : "false:false:in-process:in-process");
  });
}

test("select typeahead is scoped and preserves an empty selection", async ({
  page,
}) => {
  const trigger = page.getByRole("combobox", {
    name: "Selection",
    exact: true,
  });
  await trigger.click();
  await expect(page.getByRole("option", { name: "One", exact: true })).toBeFocused();
  await page.keyboard.press("n");
  // Radix defers typeahead focus; Enter selects the currently focused item.
  await expect(page.getByRole("option", { name: "Next", exact: true })).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(trigger).toHaveText("Next");
  await expect(page.getByTestId("shortcut")).toHaveText(
    "Workspace shortcut: 0",
  );
  await expect(trigger).toBeFocused();
  await trigger.click();
  await expect(
    page.getByRole("option", { name: "Disabled", exact: true }),
  ).toBeDisabled();
  await page.getByRole("option", { name: "None", exact: true }).click();
  await expect(page.getByTestId("result")).toHaveText("Selection: None");
});

test("menus support disabled items, nested arrows and focus restoration", async ({
  page,
}) => {
  const trigger = page.getByRole("button", { name: "Actions", exact: true });
  await trigger.click();
  await expect(
    page.getByRole("menuitem", { name: "Unavailable action" }),
  ).toBeDisabled();
  await page
    .getByRole("menuitem", { name: "Export", exact: true })
    .press("ArrowRight");
  const item = page.getByRole("menuitem", { name: "Text", exact: true });
  await expect(item).toBeFocused();
  await item.press("Enter");
  await expect(page.getByTestId("result")).toHaveText("Exported text");
  await expect(trigger).toBeFocused();
});

test("popover forms stay open during editing and restore their trigger", async ({
  page,
}) => {
  const trigger = page.getByRole("button", {
    name: "Edit settings",
    exact: true,
  });
  await trigger.click();
  const field = page.getByRole("textbox", {
    name: "Setting name",
    exact: true,
  });
  await field.fill("edited");
  await page
    .getByRole("button", { name: "Apply settings", exact: true })
    .click();
  await expect(field).toHaveValue("edited");
  await expect(page.getByTestId("result")).toHaveText("Settings applied");
  await field.press("Escape");
  await expect(field).toBeHidden();
  await expect(trigger).toBeFocused();
});

test("checks, switches, segmented choices, sliders and commands work by keyboard", async ({
  page,
}) => {
  const checkbox = page.getByRole("checkbox", {
    name: "Include files",
    exact: true,
  });
  await expect(checkbox).toHaveAttribute("aria-checked", "mixed");
  await checkbox.press("Space");
  await page
    .getByRole("switch", { name: "Enable feature", exact: true })
    .press("Space");
  await page
    .getByRole("radio", { name: "List", exact: true })
    .press("ArrowRight");
  const tree = page.getByRole("radio", { name: "Tree", exact: true });
  await expect(tree).toBeFocused();
  await tree.press("Space");
  await expect(page.getByTestId("state")).toHaveText("true · true · tree");
  await page.getByRole("slider").press("ArrowRight");
  await expect(page.getByRole("slider")).toHaveAttribute("aria-valuenow", "13");
  const command = page.getByRole("combobox", {
    name: "Find command",
    exact: true,
  });
  await command.fill("bet");
  await command.press("Enter");
  await expect(page.getByTestId("result")).toHaveText("Command beta");
  await page
    .getByRole("button", { name: "Advanced settings", exact: true })
    .click();
  await expect(
    page.getByText("Advanced content", { exact: true }),
  ).toBeVisible();
});

test("queued dialogs keep results and focus through transitions and cancellation", async ({
  page,
}) => {
  const trigger = page.getByRole("button", {
    name: "Queue dialogs",
    exact: true,
  });
  await trigger.click();
  await expect(
    page.getByRole("button", { name: "Cancel", exact: true }),
  ).toBeFocused();
  await page.getByRole("button", { name: "Confirm", exact: true }).click();
  const field = page.getByRole("textbox", { name: "Queued name", exact: true });
  await expect(field).toBeFocused();
  await field.fill("verified");
  await page.getByRole("button", { name: "OK", exact: true }).click();
  await expect(page.getByTestId("result")).toHaveText("Queue: true, verified");
  await expect(trigger).toBeFocused();
  await page.getByRole("button", { name: "Prompt", exact: true }).click();
  await page
    .getByRole("textbox", { name: "Name", exact: true })
    .press("Escape");
  await expect(page.getByTestId("result")).toHaveText("Prompt: cancelled");
});

test("terminal CLI menu filters host availability and keeps preset options", async ({ page }) => {
  await page.getByRole("button", { name: "Terminal presets", exact: true }).click();
  await expect(page.getByRole("menuitem", { name: "Claude Code", exact: true })).toBeVisible();
  await expect(page.getByRole("menuitem", { name: "Configured CLI", exact: true })).toBeVisible();
  await expect(page.getByRole("menuitem", { name: "Pi", exact: true }).locator("svg")).toBeVisible();
  await expect(page.getByRole("menuitem", { name: "Aider", exact: true })).toHaveCount(0);
  await page.getByRole("menuitem", { name: "Claude Code", exact: true }).press("ArrowRight");
  await page.getByRole("menuitem", { name: "Resume last session", exact: true }).press("Enter");
  await expect(page.getByTestId("result")).toHaveText("Launch: '/fixture/claude' --resume");
});

test("terminal CLI detection failures remain retryable", async ({ page }) => {
  await page.evaluate(() => { window.crab.orchestrator.detectAgents = async () => ({ success: false, error: "Host probe failed" }); });
  await page.getByRole("button", { name: "Terminal presets", exact: true }).click();
  await expect(page.getByRole("menu").getByRole("status")).toContainText("Could not detect agent CLIs");
  await expect(page.getByRole("menuitem", { name: "Claude Code", exact: true })).toHaveCount(0);
  await page.evaluate(() => { window.crab.orchestrator.detectAgents = async () => ({ success: true, agents: [] }); });
  await page.getByRole("menuitem", { name: "Refresh CLI list", exact: true }).click();
  await expect(page.getByRole("menu").getByRole("status")).toContainText("No agent CLIs found");
  await expect(page.getByRole("menuitem", { name: "Add custom command…", exact: true })).toBeEnabled();
});

test("terminal custom commands persist and launch through the shared menu", async ({
  page,
}) => {
  await page
    .getByRole("button", { name: "Terminal presets", exact: true })
    .click();
  await page
    .getByRole("menuitem", { name: "Add custom command…", exact: true })
    .click();
  const dialog = page.getByRole("dialog", {
    name: "Add custom command",
    exact: true,
  });
  await dialog.getByLabel("Name", { exact: true }).fill("Local check");
  await dialog
    .getByLabel("Shell command", { exact: true })
    .fill("echo verified");
  await dialog.getByRole("button", { name: "Save", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Terminal presets", exact: true }),
  ).toBeFocused();
  await page.reload();
  await page
    .getByRole("button", { name: "Terminal presets", exact: true })
    .click();
  await page
    .getByRole("menuitem", { name: "Local check", exact: true })
    .press("ArrowRight");
  await page
    .getByRole("menuitem", { name: "Run Local check", exact: true })
    .press("Enter");
  await expect(page.getByTestId("result")).toHaveText("Launch: echo verified");
});

test("notifications preserve actions and dismiss through the shadcn toaster", async ({
  page,
}) => {
  await page.getByRole("button", { name: "Notify", exact: true }).click();
  await page.getByRole("button", { name: "Undo", exact: true }).click();
  await expect(page.getByTestId("result")).toHaveText("Undo clicked");
  await expect(page.getByText("Saved", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Close toast", exact: true }).click();
  await expect(page.getByText("Saved", { exact: true })).toBeHidden();
});

for (const theme of ["light", "dark", "hc-light", "hc-dark"] as const) {
  test(`${theme}: toast text remains readable when the system theme differs`, async ({ page }) => {
    await page.emulateMedia({ colorScheme: theme.endsWith("light") ? "dark" : "light" });
    await page.goto(`/tests/fixtures/shadcn-controls.html?theme=${theme}`);
    // Packaged renderer CSS loads before Sonner injects its own stylesheet.
    await page.addStyleTag({ path: require.resolve("sonner/dist/styles.css") });
    await page.getByRole("button", { name: "Notify", exact: true }).click();
    await expect(page.locator("[data-description]")).toHaveText("Your changes are saved.");
    await expect(page.locator("[data-sonner-toast]")).toHaveCSS("opacity", "1");
    const result = await new AxeBuilder({ page })
      .include("[data-sonner-toaster]")
      .withRules(["color-contrast"])
      .analyze();
    expect(result.violations.map(({ id, nodes }) => ({ id, nodes: nodes.map(node => node.failureSummary) }))).toEqual([]);
    await page.screenshot({ path: test.info().outputPath(`toast-${theme}.png`) });
  });
}

test("workspace dividers stay visible and resize by pointer and keyboard", async ({ page }) => {
  await page.goto("/tests/fixtures/shadcn-controls.html?theme=light");
  const divider = page.getByRole("separator", { name: "Resize workspace sidebar", exact: true });
  const line = await divider.evaluate(element => {
    const style = getComputedStyle(element, "::after");
    return { color: style.backgroundColor, width: style.width };
  });
  expect(line.color).not.toBe("rgba(0, 0, 0, 0)");
  expect(line.width).toBe("1px");
  await divider.scrollIntoViewIfNeeded();
  const before = Number(await divider.getAttribute("aria-valuenow"));
  const box = await divider.boundingBox();
  if (!box) throw new Error("Workspace divider has no drag target");
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + 60, box.y + box.height / 2, { steps: 5 });
  await page.mouse.up();
  await expect.poll(async () => Number(await divider.getAttribute("aria-valuenow"))).toBeGreaterThan(before);
  const dragged = Number(await divider.getAttribute("aria-valuenow"));
  await divider.focus();
  await divider.press("ArrowRight");
  await expect.poll(async () => Number(await divider.getAttribute("aria-valuenow"))).toBeGreaterThan(dragged);
  const persisted = await page.evaluate(() => localStorage.getItem("crab.sidebarPct"));
  await page.reload();
  expect(await page.evaluate(() => localStorage.getItem("crab.sidebarPct"))).toBe(persisted);
  const primary = page.getByRole("navigation", { name: "Primary", exact: true });
  await primary.getByRole("button", { name: "Settings", exact: true }).click();
  await expect(divider).toBeHidden();
  await primary.getByRole("button", { name: "Files", exact: true }).click();
  await expect(divider).toBeVisible();
  await expect.poll(async () => Number(await divider.getAttribute("aria-valuenow"))).toBeCloseTo(Number(persisted), 0);
  const horizontal = page.getByRole("separator", { name: "Resize workflow output", exact: true });
  await expect(horizontal).toHaveAttribute("aria-orientation", "horizontal");
  expect(await horizontal.evaluate(element => getComputedStyle(element, "::after").height)).toBe("1px");
});

test("workspace and workflow tabs share borderless selection styles", async ({ page }) => {
  await page.goto("/tests/fixtures/shadcn-controls.html?theme=light");
  const explorer = page.getByRole("navigation", { name: "Files", exact: true }).getByRole("button", { name: "Explorer", exact: true });
  const editor = page.getByRole("tab", { name: "Editor", exact: true });
  const appearance = (element: Element) => {
    const css = getComputedStyle(element);
    return [css.borderTopWidth, css.borderRightWidth, css.borderBottomWidth, css.borderLeftWidth, css.boxShadow, css.backgroundColor, css.color];
  };
  await expect.poll(() => explorer.evaluate(appearance)).toEqual(await editor.evaluate(appearance));
  expect((await editor.evaluate(appearance)).slice(0, 4)).toEqual(["0px", "0px", "0px", "0px"]);
  await editor.focus();
  await editor.press("ArrowRight");
  await expect(page.getByRole("tab", { name: "Status", exact: true })).toBeFocused();
  await expect(page.getByRole("tabpanel")).toHaveText("Workflow status");
});

test("controls expose accessible names and contrast", async ({ page }) => {
  const result = await new AxeBuilder({ page })
    .withTags(["wcag2a", "wcag2aa"])
    .analyze();
  expect(
    result.violations.map((violation) => ({
      id: violation.id,
      nodes: violation.nodes.map((node) => node.failureSummary),
    })),
  ).toEqual([]);
  await page.screenshot({
    path: test.info().outputPath("shadcn-controls.png"),
  });
});

// Radix dialogs portal into the document after mount; their content and
// safeguards must be exercised in a browser rather than static server markup.
test("paste review exposes preview, safe initial focus and cancellation", async ({ page }) => {
  const trigger = page.getByRole("button", { name: "Review multi paste", exact: true });
  await trigger.click();
  const dialog = page.getByRole("alertdialog", { name: "Review paste content", exact: true });
  await expect(dialog).toBeVisible();
  await expect(dialog.getByText("42 lines", { exact: true })).toBeVisible();
  await expect(dialog.getByLabel("Paste preview")).toHaveText("first\n...\nlast");
  await expect(dialog.getByRole("checkbox")).toHaveCount(0);
  await expect(dialog.getByRole("button", { name: "Paste", exact: true })).toBeEnabled();
  const cancel = dialog.getByRole("button", { name: "Cancel", exact: true });
  await expect(cancel).toBeFocused();
  await cancel.press("Shift+Tab");
  await expect(dialog.getByRole("button", { name: "Paste", exact: true })).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("result")).toHaveText("Paste cancelled");
  await expect(trigger).toBeFocused();
});

test("paste review shows control character badges and highlights", async ({ page }) => {
  await page.getByRole("button", { name: "Review control paste", exact: true }).click();
  const dialog = page.getByRole("alertdialog", { name: "Review paste content", exact: true });
  await expect(dialog.getByLabel("Paste preview")).toHaveText("hello^Cworld^[end");
  await expect(dialog.locator('[title="Control character: ^C"]')).toBeVisible();
  await expect(dialog.getByText("^[", { exact: true }).last()).toBeVisible();
});

test("dangerous paste stays blocked until the user acknowledges its risks", async ({ page }) => {
  await page.getByRole("button", { name: "Review dangerous paste", exact: true }).click();
  const dialog = page.getByRole("alertdialog", { name: "Dangerous paste detected", exact: true });
  await expect(dialog).toContainText("dangerous command pattern");
  await expect(dialog).toContainText("control character");
  await expect(dialog).toContainText("Multi-line paste");
  const paste = dialog.getByRole("button", { name: "Paste", exact: true });
  await expect(paste).toBeDisabled();
  await dialog.getByRole("checkbox", { name: "I understand the risk and want to paste this content", exact: true }).press("Space");
  await expect(paste).toBeEnabled();
  await paste.click();
  await expect(dialog).toBeHidden();
  await expect(page.getByTestId("result")).toHaveText("Paste confirmed");
});


test("chat composer shares one frame while preserving multiline input, attachments and send/stop", async ({ page }) => {
  const composer = page.getByRole("group", { name: "Message composer", exact: true });
  const message = composer.getByRole("textbox", { name: "Chat message input" });
  const initialHeight = await message.evaluate(field => field.clientHeight);
  await message.fill("Line\n".repeat(10));
  await expect.poll(() => message.evaluate(field => field.clientHeight)).toBeGreaterThan(initialHeight);
  await message.fill("First line");
  await message.press("Shift+Enter");
  await message.pressSequentially("Second line");
  expect(await message.evaluate(field => ({
    border: getComputedStyle(field).borderTopWidth,
    frame: getComputedStyle(field.closest('[role="group"]')!).borderTopWidth,
  }))).toEqual({ border: "0px", frame: "1px" });
  await composer.getByRole("button", { name: "README.md", exact: true }).click();
  await expect(composer.getByRole("button", { name: "Remove README.md" })).toBeVisible();
  await composer.getByRole("button", { name: /^Instant mode/ }).click();
  await message.press("ControlOrMeta+Enter");
  await expect(page.getByTestId("composer-result")).toHaveText("First line\nSecond line · plan · README.md");
  await expect(message).toBeDisabled();
  await expect(message).toHaveValue("");
  await composer.getByRole("button", { name: "Stop agent", exact: true }).click();
  await expect(message).toBeEnabled();
});

test("view toolbars keep usable targets, keyboard actions and toggle states in narrow panels", async ({ page }) => {
  await page.setViewportSize({ width: 380, height: 900 });
  const toolbar = page.getByRole("toolbar", { name: "Repository files", exact: true });
  const refresh = toolbar.getByRole("button", { name: "Refresh files", exact: true });
  await refresh.press("Enter");
  await expect(page.getByTestId("result")).toHaveText("Toolbar: refresh");
  const amend = toolbar.getByRole("button", { name: "Amend commit", exact: true });
  await amend.press("Space");
  await expect(amend).toHaveAttribute("aria-pressed", "true");
  await expect(toolbar.getByRole("button", { name: "Delete files", exact: true })).toBeDisabled();
  for (const button of await toolbar.getByRole("button").all()) {
    const box = await button.boundingBox();
    expect(box && box.width >= 32 && box.height >= 32).toBeTruthy();
  }
  expect(await toolbar.evaluate(element => element.scrollWidth <= element.clientWidth)).toBe(true);
});
