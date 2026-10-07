/**
 * Google Analytics for the Tauri desktop shell, via the dedicated app web
 * stream (G-BGD43VPDEJ — separate from the website's G-83S2Y2V9DE).
 *
 * gtag.js normally keys reports off the page's hostname, which inside Tauri
 * is the opaque webview origin (`tauri://localhost` / `http://tauri.localhost`)
 * and lands as noise in GA. GA does not verify that page_location resolves,
 * so we present every hit under the canonical virtual host
 * `https://app.wowsp.langyo.xyz` — a label that need not actually resolve —
 * which keeps the app stream's reports grouped under one readable domain and
 * distinct from the website stream.
 *
 * The loader is injected at runtime (not via index.html) so it only ever runs
 * in the Tauri shell with a release build; dev sessions and the plain-browser
 * mock mode stay analytics-free. It also waits for the first-run wizard
 * (`wowsp-onboarding-completed`, set by OnboardingWizard's finish(), which
 * calls initAnalytics() again) and honors the Settings → About opt-out
 * (`wowsp-analytics-disabled`), matching docs/{lang}/license/usage-telemetry.md. CSP in tauri.conf.json must allow
 * googletagmanager.com (script/connect) and google-analytics.com (collect).
 */

const APP_MEASUREMENT_ID = "G-BGD43VPDEJ";

/** Virtual canonical origin every app hit is reported under. */
const APP_PAGE_ORIGIN = "https://app.wowsp.langyo.xyz";

/** Written by OnboardingWizard's finish(); nothing is sent before it. */
const ONBOARDING_COMPLETED_KEY = "wowsp-onboarding-completed";

/** "1" = the user turned telemetry off in Settings → About. */
const DISABLED_KEY = "wowsp-analytics-disabled";

/** gtag.js's documented per-property kill switch (`window['ga-disable-<id>']`). */
const GA_DISABLE_FLAG = `ga-disable-${APP_MEASUREMENT_ID}`;

let loaded = false;

function readKey(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

/** Whether the user has left telemetry on (the default). */
export function isAnalyticsEnabled(): boolean {
  return readKey(DISABLED_KEY) !== "1";
}

declare global {
  interface Window {
    dataLayer?: unknown[];
    gtag?: (...args: unknown[]) => void;
  }
}

export function initAnalytics(): void {
  if (typeof window === "undefined") return;
  if (!("__TAURI_INTERNALS__" in window)) return;
  if (import.meta.env.DEV) return;
  if (loaded) return;
  if (readKey(ONBOARDING_COMPLETED_KEY) === null) return;
  if (!isAnalyticsEnabled()) return;
  loaded = true;
  (window as unknown as Record<string, unknown>)[GA_DISABLE_FLAG] = false;

  window.dataLayer = window.dataLayer || [];
  window.gtag = function gtag(...args: unknown[]) {
    window.dataLayer!.push(args);
  };
  window.gtag("js", new Date());
  window.gtag("config", APP_MEASUREMENT_ID, {
    // Anchor every hit on the virtual host so GA reports group by path
    // instead of the opaque tauri://localhost origin.
    page_location: `${APP_PAGE_ORIGIN}${window.location.pathname}`,
  });

  const script = document.createElement("script");
  script.async = true;
  script.src = `https://www.googletagmanager.com/gtag/js?id=${APP_MEASUREMENT_ID}`;
  document.head.appendChild(script);
}

/** Persist the Settings → About telemetry switch. Turning it off also
 *  raises gtag's kill switch so the running session stops sending at once;
 *  turning it on loads the tag if this session has not yet. */
export function setAnalyticsEnabled(enabled: boolean): void {
  try {
    if (enabled) localStorage.removeItem(DISABLED_KEY);
    else localStorage.setItem(DISABLED_KEY, "1");
  } catch {
    // Unpersistable storage: the session-level switch below still applies.
  }
  if (typeof window === "undefined") return;
  (window as unknown as Record<string, unknown>)[GA_DISABLE_FLAG] = !enabled;
  if (enabled) initAnalytics();
}

/** Report a client-side route change as a page_view on the virtual host. */
export function trackPageView(title: string, path: string): void {
  if (!loaded || !isAnalyticsEnabled()) return;
  window.gtag?.("event", "page_view", {
    page_title: title,
    page_path: path,
    page_location: `${APP_PAGE_ORIGIN}${path}`,
  });
}
