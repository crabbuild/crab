import { expect, test } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

test.beforeEach(async ({ page }) => {
  await page.goto("/tests/fixtures/shadcn-controls.html");
});

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
