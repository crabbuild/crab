import { expect, test } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

const fixture = "/tests/fixtures/skills.html";

test("agent folder summary explains detection, names paths and supports scan retry", async ({
  page,
}) => {
  await page.goto(`${fixture}?scan-error`);
  await page.getByRole("button", { name: "Agent scan failed" }).click();
  await expect(page.getByRole("alert")).toHaveText("Folder scan unavailable");
  await page.getByRole("button", { name: "Scan again" }).click();
  await expect(page.getByText("Codex CLI", { exact: true })).toBeVisible();
  await expect(
    page.getByText("/fixture/.codex", { exact: true }),
  ).toBeVisible();
  await expect(page.getByText("Found", { exact: true })).toBeVisible();
  await expect(page.getByText("Not found", { exact: true })).toBeVisible();
  await expect(
    page.getByText("Crab checks local configuration folders.", {
      exact: false,
    }),
  ).toBeVisible();
});

test("list failure exits loading and retry restores the actionable empty state", async ({
  page,
}) => {
  await page.goto(`${fixture}?list-error`);
  await expect(page.getByRole("alert")).toContainText("Library unavailable");
  await page.getByRole("button", { name: "Retry loading skills" }).click();
  await expect(
    page.getByText("Build your skills library", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "New skill", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("Skills managed outside Crab", { exact: false }),
  ).toBeVisible();
});

test("create recovers from transport failure, selects the skill, and keeps chat availability current", async ({
  page,
}) => {
  await page.goto(`${fixture}?create-error`);
  await page.getByRole("button", { name: "New skill", exact: true }).click();
  await page
    .getByRole("textbox", { name: "Name", exact: true })
    .fill("my-review");
  await page
    .getByRole("textbox", { name: "Description", exact: true })
    .fill("Review changes before merging");
  await page.getByRole("button", { name: "Create skill", exact: true }).click();
  await expect(page.getByRole("alert")).toHaveText("Disk write failed");
  await page.getByRole("button", { name: "Create skill", exact: true }).click();
  const detail = page.getByRole("article", { name: "Skill details" });
  await expect(
    detail.getByRole("heading", { name: "my-review", exact: true }),
  ).toBeVisible();
  const toggle = page.getByRole("switch", { name: "Available in Crab chat" });
  await toggle.click();
  await expect(toggle).not.toBeChecked();
  await expect(
    page.getByRole("complementary").getByText("Hidden from chat"),
  ).toBeVisible();
  await expect(detail.getByText("Linked", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Edit skill" }).click();
  await page
    .getByRole("textbox", { name: "Instructions", exact: true })
    .fill("# Updated\n\nReadable instructions.");
  await page.getByRole("button", { name: "Save changes" }).click();
  await expect(
    detail.getByRole("heading", { name: "Updated", exact: true }),
  ).toBeVisible();
});

test("GitHub shorthand installs and opens its detail", async ({ page }) => {
  await page.goto(fixture);
  await page
    .getByRole("button", { name: "Install from GitHub", exact: true })
    .click();
  await page
    .getByRole("textbox", { name: "GitHub URL or path" })
    .fill("owner/repo/skills/review/SKILL.md");
  await page
    .getByRole("button", { name: "Install skill", exact: true })
    .click();
  await expect(
    page.getByRole("heading", { name: "installed-skill", exact: true }),
  ).toBeVisible();
});

test("search recovers from no matches and stale detail cannot replace the current selection", async ({
  page,
}) => {
  await page.goto(`${fixture}?populated&slow-detail`);
  const search = page.getByRole("textbox", { name: "Search skills" });
  await search.fill("missing skill");
  await expect(
    page.getByText("No matching skills", { exact: true }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Clear search" }).click();
  await page.getByRole("button", { name: "code-review", exact: false }).click();
  await expect(
    page.getByRole("status").filter({ hasText: "Loading skill…" }),
  ).toBeVisible();
  await page.getByRole("button", { name: "write-docs", exact: false }).click();
  await expect(
    page
      .getByRole("article")
      .getByRole("heading", { name: "write-docs", exact: true }),
  ).toBeVisible();
  await expect(async () => {
    await page.waitForTimeout(900);
    await expect(
      page
        .getByRole("article")
        .getByRole("heading", { name: "write-docs", exact: true }),
    ).toBeVisible();
  }).toPass();
});

test("detail load errors offer retry", async ({ page }) => {
  await page.goto(`${fixture}?populated&detail-error`);
  await page.getByRole("button", { name: "code-review", exact: false }).click();
  await expect(page.getByRole("alert")).toContainText("Skill file unavailable");
  await page.getByRole("button", { name: "Retry opening skill" }).click();
  await expect(
    page
      .getByRole("article")
      .getByRole("heading", { name: "code-review", exact: true }),
  ).toBeVisible();
});

for (const query of ["sync-error", "sync-conflict"])
  test(`${query} is visible and releases the sync action`, async ({ page }) => {
    await page.goto(`${fixture}?populated&${query}`);
    await page
      .getByRole("button", { name: "Sync skills", exact: true })
      .click();
    await expect(
      page.getByRole("button", { name: "Sync skills", exact: true }),
    ).toBeEnabled();
    await expect(
      page.getByText(
        query === "sync-error" ? "Sync failed" : "Some links need attention",
        { exact: true },
      ),
    ).toBeVisible();
  });

for (const theme of ["light", "dark", "hc-light", "hc-dark"])
  test(`skills are readable and accessible in ${theme}`, async ({ page }) => {
    await page.goto(`${fixture}?populated&theme=${theme}`);
    await page
      .getByRole("button", { name: "code-review", exact: false })
      .click();
    await expect(
      page
        .getByRole("article")
        .getByRole("heading", { name: "Review", exact: true }),
    ).toBeVisible();
    expect(
      (await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa"]).analyze())
        .violations,
    ).toEqual([]);
    await page.screenshot({
      path: test.info().outputPath(`skills-${theme}.png`),
    });
    await page.getByRole("button", { name: /^Agent folders/ }).click();
    expect(
      (await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa"]).analyze())
        .violations,
    ).toEqual([]);
  });

test("native skills UI writes instructions, projects links, persists chat visibility, preserves conflicts and deletes owned links", async ({
  baseURL,
}) => {
  test.setTimeout(60_000);
  const { _electron } = await import("@playwright/test");
  const fs = await import("node:fs");
  const os = await import("node:os");
  const path = await import("node:path");
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "crab-skills-ui-"));
  fs.writeFileSync(path.join(root, "owned-skills-fixture"), "owned");
  fs.mkdirSync(path.join(root, ".codex"));
  const app = await _electron.launch({
    args: [
      path.resolve(__dirname, "../fixtures/skills-main.cjs"),
      root,
      `${baseURL}${fixture}`,
    ],
  });
  try {
    const page = await app.firstWindow();
    await page.getByRole("button", { name: "New skill", exact: true }).click();
    await page
      .getByRole("textbox", { name: "Name", exact: true })
      .fill("native-review");
    await page
      .getByRole("textbox", { name: "Description", exact: true })
      .fill("Verify native skill ownership");
    await page
      .getByRole("textbox", { name: "Instructions (optional)", exact: true })
      .fill("# Native instructions\n\nRead and test changes.");
    await page
      .getByRole("button", { name: "Create skill", exact: true })
      .click();
    const detail = page.getByRole("article", { name: "Skill details" });
    await expect(
      detail.getByRole("heading", { name: "native-review", exact: true }),
    ).toBeVisible();
    const canonical = path.join(root, ".crab/desktop/skills/native-review");
    const link = path.join(root, ".codex/skills/native-review");
    expect(fs.readFileSync(path.join(canonical, "SKILL.md"), "utf8")).toContain(
      "# Native instructions",
    );
    expect(fs.realpathSync(link)).toBe(fs.realpathSync(canonical));
    await page.getByRole("switch", { name: "Available in Crab chat" }).click();
    await expect(
      page.getByRole("switch", { name: "Available in Crab chat" }),
    ).not.toBeChecked();
    expect(fs.lstatSync(link).isSymbolicLink()).toBe(true);
    await page.getByRole("button", { name: "Edit skill" }).click();
    await page
      .getByRole("textbox", { name: "Instructions", exact: true })
      .fill("# Native update\n\nPersisted Markdown.");
    await page.getByRole("button", { name: "Save changes" }).click();
    await expect(
      detail.getByRole("heading", { name: "Native update", exact: true }),
    ).toBeVisible();
    expect(fs.readFileSync(path.join(canonical, "SKILL.md"), "utf8")).toContain(
      "Persisted Markdown.",
    );
    await page.reload();
    await page
      .getByRole("button", { name: /native-review.*Hidden from chat/ })
      .click();
    await expect(
      page.getByRole("switch", { name: "Available in Crab chat" }),
    ).not.toBeChecked();
    fs.unlinkSync(link);
    fs.mkdirSync(link);
    fs.writeFileSync(path.join(link, "SKILL.md"), "Existing agent skill");
    await page
      .getByRole("button", { name: "Sync skills", exact: true })
      .click();
    await expect(
      page.getByText("Some links need attention", { exact: true }),
    ).toBeVisible();
    await expect(detail.getByText("Needs sync", { exact: true })).toBeVisible();
    expect(fs.readFileSync(path.join(link, "SKILL.md"), "utf8")).toBe(
      "Existing agent skill",
    );
    fs.rmSync(link, { recursive: true });
    await page
      .getByRole("button", { name: "Sync skills", exact: true })
      .click();
    await expect(detail.getByText("Linked", { exact: true })).toBeVisible();
    expect(fs.realpathSync(link)).toBe(fs.realpathSync(canonical));
    await page.screenshot({
      path: test.info().outputPath("skills-native.png"),
    });
    await page
      .getByRole("button", { name: "Delete skill", exact: true })
      .click();
    await page.getByRole("button", { name: "Confirm", exact: true }).click();
    await expect(
      page.getByText("Build your skills library", { exact: true }),
    ).toBeVisible();
    expect(fs.existsSync(canonical)).toBe(false);
    expect(fs.existsSync(link)).toBe(false);
  } finally {
    await app.close();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("empty state and dialogs stay accessible at a narrow desktop width", async ({
  page,
}) => {
  await page.setViewportSize({ width: 700, height: 800 });
  await page.goto(fixture);
  await expect(
    page.getByText("Build your skills library", { exact: true }),
  ).toBeVisible();
  expect(
    (await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa"]).analyze())
      .violations,
  ).toEqual([]);
  await page.getByRole("button", { name: "New skill", exact: true }).click();
  const name = page.getByRole("textbox", { name: "Name", exact: true });
  await expect(name).toBeFocused();
  await name.fill("Invalid Name");
  await page
    .getByRole("textbox", { name: "Description", exact: true })
    .fill("Description");
  await page.getByRole("button", { name: "Create skill", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("Use 1–64 lowercase");
  expect(
    (await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa"]).analyze())
      .violations,
  ).toEqual([]);
  await page.keyboard.press("Escape");
  await expect(
    page.getByRole("button", { name: "New skill", exact: true }),
  ).toBeFocused();
  await page
    .getByRole("button", { name: "Install from GitHub", exact: true })
    .click();
  await expect(
    page.getByRole("textbox", { name: "GitHub URL or path" }),
  ).toBeFocused();
  expect(
    (await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa"]).analyze())
      .violations,
  ).toEqual([]);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
});
