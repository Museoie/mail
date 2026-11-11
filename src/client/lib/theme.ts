import { useEffect, useState } from "react";

/** The reader's choice in Settings. "system" follows the device. */
export type ThemeSetting = "system" | "light" | "dark";
export type Theme = "light" | "dark";

export const sanitizeThemeSetting = (value: unknown): ThemeSetting =>
  value === "light" || value === "dark" ? value : "system";

/** The theme actually shown for a setting and the device preference. */
export const resolveTheme = (
  setting: ThemeSetting,
  systemPrefersLight: boolean
): Theme => {
  if (setting === "system") return systemPrefersLight ? "light" : "dark";
  return setting;
};

const LIGHT_QUERY = "(prefers-color-scheme: light)";

const systemPrefersLight = () =>
  typeof window !== "undefined" &&
  typeof window.matchMedia === "function" &&
  window.matchMedia(LIGHT_QUERY).matches;

/**
 * The theme in effect, following device changes live. The page's own CSS
 * switches by itself; this is for the mail viewer's iframe, which CSS
 * variables and the `data-theme` attribute don't reach.
 */
export const useResolvedTheme = (setting: ThemeSetting): Theme => {
  const [prefersLight, setPrefersLight] = useState(systemPrefersLight);
  useEffect(() => {
    if (typeof window.matchMedia !== "function") return;
    const query = window.matchMedia(LIGHT_QUERY);
    const onChange = () => setPrefersLight(query.matches);
    query.addEventListener?.("change", onChange);
    return () => query.removeEventListener?.("change", onChange);
  }, []);
  return resolveTheme(setting, prefersLight);
};

/** Reflect the setting on <html>, where the stylesheet reads it. */
export const applyThemeSetting = (setting: ThemeSetting) => {
  const root = document.documentElement;
  if (setting === "system") root.removeAttribute("data-theme");
  else root.setAttribute("data-theme", setting);
};
