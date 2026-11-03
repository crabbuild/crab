import { describe, it, expect } from "vitest";
import { isValidSkillName, parseFrontmatter, generateSkillMd } from "../electron/skill-validation";

describe("isValidSkillName", () => {
  it("accepts valid lowercase names", () => {
    expect(isValidSkillName("my-skill")).toBe(true);
    expect(isValidSkillName("code-review")).toBe(true);
    expect(isValidSkillName("a")).toBe(true);
    expect(isValidSkillName("deploy-to-prod")).toBe(true);
  });

  it("accepts names with numbers", () => {
    expect(isValidSkillName("python3-runner")).toBe(true);
    expect(isValidSkillName("fix-404-errors")).toBe(true);
  });

  it("rejects empty strings", () => {
    expect(isValidSkillName("")).toBe(false);
  });

  it("rejects names with uppercase", () => {
    expect(isValidSkillName("My-Skill")).toBe(false);
    expect(isValidSkillName("CODE-REVIEW")).toBe(false);
  });

  it("rejects names with consecutive hyphens", () => {
    expect(isValidSkillName("my--skill")).toBe(false);
  });

  it("rejects names starting with hyphen", () => {
    expect(isValidSkillName("-my-skill")).toBe(false);
  });

  it("allows trailing hyphen (optional last char makes it mid-group)", () => {
    // The regex /^[a-z0-9][a-z0-9-]{0,62}[a-z0-9]?$/ allows a trailing hyphen
    // because the middle group greedily consumes it and the last char is optional.
    expect(isValidSkillName("my-skill-")).toBe(true);
  });

  it("rejects names with spaces", () => {
    expect(isValidSkillName("my skill")).toBe(false);
  });

  it("rejects names with special chars", () => {
    expect(isValidSkillName("my_skill")).toBe(false);
    expect(isValidSkillName("my.skill")).toBe(false);
    expect(isValidSkillName("my@skill")).toBe(false);
  });

  it("rejects names over 64 chars", () => {
    expect(isValidSkillName("a".repeat(65))).toBe(false);
  });

  it("accepts names at exactly 64 chars", () => {
    expect(isValidSkillName("a".repeat(64))).toBe(true);
  });

  it("allows leading digits (first char class includes 0-9)", () => {
    // The regex /^[a-z0-9]/ accepts digits as the first character.
    expect(isValidSkillName("123skill")).toBe(true);
  });
});

describe("parseFrontmatter", () => {
  it("parses standard frontmatter with double-quoted values", () => {
    const content = [
      "---",
      'name: "My Skill"',
      'description: "A test skill for validation"',
      "---",
      "",
      "# Body content",
      "Some markdown here.",
    ].join("\n");

    const result = parseFrontmatter(content);
    expect(result.frontmatter.name).toBe("My Skill");
    expect(result.frontmatter.description).toBe("A test skill for validation");
    expect(result.body).toContain("# Body content");
    expect(result.body).toContain("Some markdown here.");
  });

  it("handles unquoted values", () => {
    const content = [
      "---",
      "name: simple-skill",
      "description: Simple description",
      "---",
      "",
      "Body here.",
    ].join("\n");

    const result = parseFrontmatter(content);
    expect(result.frontmatter.name).toBe("simple-skill");
    expect(result.frontmatter.description).toBe("Simple description");
  });

  it("handles content with no frontmatter", () => {
    const content = "# Just a heading\n\nNo frontmatter here.";
    const result = parseFrontmatter(content);
    expect(result.frontmatter.name).toBe("");
    expect(result.frontmatter.description).toBe("");
    expect(result.body).toBe(content);
  });

  it("handles escaped quotes in double-quoted values", () => {
    const content = [
      "---",
      'name: "Skill with \\"quotes\\""',
      'description: "Desc with \\\\ backslash"',
      "---",
      "",
      "Body.",
    ].join("\n");

    const result = parseFrontmatter(content);
    expect(result.frontmatter.name).toBe('Skill with "quotes"');
    expect(result.frontmatter.description).toBe("Desc with \\ backslash");
  });

  it("handles single-quoted values", () => {
    const content = [
      "---",
      "name: 'single-quoted-name'",
      "description: 'Single quoted desc'",
      "---",
      "",
      "Body.",
    ].join("\n");

    const result = parseFrontmatter(content);
    expect(result.frontmatter.name).toBe("single-quoted-name");
    expect(result.frontmatter.description).toBe("Single quoted desc");
  });

  it("handles single quotes with doubled escaping", () => {
    const content = [
      "---",
      "name: 'it''s a skill'",
      "description: 'desc'",
      "---",
      "",
      "Body.",
    ].join("\n");

    const result = parseFrontmatter(content);
    expect(result.frontmatter.name).toBe("it's a skill");
  });

  it("extracts optional frontmatter fields", () => {
    const content = [
      "---",
      'name: "test"',
      'description: "test desc"',
      'license: "MIT"',
      'compatibility: "all"',
      'allowed-tools: "bash,git"',
      "---",
      "",
      "Body.",
    ].join("\n");

    const result = parseFrontmatter(content);
    expect(result.frontmatter.license).toBe("MIT");
    expect(result.frontmatter.compatibility).toBe("all");
    expect(result.frontmatter["allowed-tools"]).toBe("bash,git");
  });

  it("ignores empty lines in frontmatter", () => {
    const content = [
      "---",
      'name: "test"',
      "",
      'description: "desc"',
      "---",
      "",
      "Body.",
    ].join("\n");

    const result = parseFrontmatter(content);
    expect(result.frontmatter.name).toBe("test");
    expect(result.frontmatter.description).toBe("desc");
  });
});

describe("generateSkillMd", () => {
  it("generates valid SKILL.md with frontmatter and body", () => {
    const result = generateSkillMd("my-skill", "A test skill", "# Custom Body\n\nHello world.");
    const parsed = parseFrontmatter(result);

    expect(parsed.frontmatter.name).toBe("my-skill");
    expect(parsed.frontmatter.description).toBe("A test skill");
    expect(parsed.body.trim()).toBe("# Custom Body\n\nHello world.");
  });

  it("generates default body when none provided", () => {
    const result = generateSkillMd("my-skill", "A test skill");
    const parsed = parseFrontmatter(result);

    expect(parsed.frontmatter.name).toBe("my-skill");
    expect(parsed.frontmatter.description).toBe("A test skill");
    expect(parsed.body).toContain("# my-skill");
    expect(parsed.body).toContain("A test skill");
  });

  it("escapes double quotes in frontmatter values", () => {
    const result = generateSkillMd('skill-with-"quotes"', 'Desc has "quotes" too');
    const parsed = parseFrontmatter(result);

    expect(parsed.frontmatter.name).toBe('skill-with-"quotes"');
    expect(parsed.frontmatter.description).toBe('Desc has "quotes" too');
  });

  it("round-trips content through parse and generate", () => {
    const original = generateSkillMd("round-trip", "Testing round trip", "# Body\n\nContent here.");
    const parsed = parseFrontmatter(original);
    const regenerated = generateSkillMd(
      parsed.frontmatter.name,
      parsed.frontmatter.description,
      parsed.body,
    );
    const reParsed = parseFrontmatter(regenerated);

    expect(reParsed.frontmatter.name).toBe("round-trip");
    expect(reParsed.frontmatter.description).toBe("Testing round trip");
    expect(reParsed.body.trim()).toBe("# Body\n\nContent here.");
  });
});
