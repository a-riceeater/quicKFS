export type ThemePreference = "system" | "light" | "dark";
export type EffectiveTheme = "light" | "dark";

export const THEME_STORAGE_KEY = "quickfs-theme";

export function isThemePreference(value: string | null): value is ThemePreference {
  return value === "system" || value === "light" || value === "dark";
}

export function effectiveTheme(
  preference: ThemePreference,
  systemPrefersDark: boolean
): EffectiveTheme {
  if (preference === "system") {
    return systemPrefersDark ? "dark" : "light";
  }
  return preference;
}

export function applyTheme(preference: ThemePreference, systemPrefersDark: boolean): void {
  const effective = effectiveTheme(preference, systemPrefersDark);
  document.documentElement.dataset.themePreference = preference;
  document.documentElement.dataset.theme = effective;
  document
    .querySelector<HTMLMetaElement>('meta[name="theme-color"]')
    ?.setAttribute("content", effective === "dark" ? "#242321" : "#f5fffb");
}
