import { describe, it, expect } from "bun:test";
import { resolveTheme, sanitizeThemeSetting } from "./theme";

describe("resolveTheme", () => {
  it("follows the device for the system setting", () => {
    expect(resolveTheme("system", true)).toBe("light");
    expect(resolveTheme("system", false)).toBe("dark");
  });

  it("keeps an explicit choice whatever the device prefers", () => {
    expect(resolveTheme("dark", true)).toBe("dark");
    expect(resolveTheme("light", false)).toBe("light");
  });
});

describe("sanitizeThemeSetting", () => {
  it("keeps known values and falls back to system", () => {
    expect(sanitizeThemeSetting("light")).toBe("light");
    expect(sanitizeThemeSetting("dark")).toBe("dark");
    expect(sanitizeThemeSetting("purple")).toBe("system");
    expect(sanitizeThemeSetting(undefined)).toBe("system");
  });
});
