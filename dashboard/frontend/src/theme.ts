/**
 * Light/dark selection.
 *
 * The chosen theme is stamped on the root element as `data-theme` and read
 * back from localStorage on the next visit. index.html stamps it again inline
 * before the bundle runs, so a light-themed dashboard does not flash dark on
 * every reload.
 */

export type Theme = "dark" | "light";

export const THEME_STORAGE_KEY = "agave-dashboard-theme";

export function readTheme(): Theme {
  try {
    return window.localStorage.getItem(THEME_STORAGE_KEY) === "light" ? "light" : "dark";
  } catch {
    // Private browsing and some embedded webviews refuse storage outright.
    return "dark";
  }
}

export function applyTheme(theme: Theme): void {
  document.documentElement.dataset.theme = theme;
  try {
    window.localStorage.setItem(THEME_STORAGE_KEY, theme);
  } catch {
    // Not being able to remember the choice is not a reason to refuse it.
  }
}
