/** Analytics gating: nothing loads before the first-run wizard completes,
 *  the Settings → About switch persists and stops a running session, and
 *  turning it back on loads the tag. The module keeps a `loaded` flag, so
 *  every case re-imports a fresh instance (vi.resetModules). */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const KILL_SWITCH = "ga-disable-G-BGD43VPDEJ";

async function freshModule() {
  vi.resetModules();
  return import("./analytics");
}

/** gtag.js script tags initAnalytics tried to inject. appendChild is
 *  intercepted so the test never fetches the real tag over the network. */
let injected: string[] = [];

function win(): Record<string, unknown> {
  return window as unknown as Record<string, unknown>;
}

beforeEach(() => {
  localStorage.clear();
  injected = [];
  vi.spyOn(document.head, "appendChild").mockImplementation(<T extends Node>(node: T): T => {
    if (node instanceof HTMLScriptElement) injected.push(node.src);
    return node;
  });
  delete win().gtag;
  delete win().dataLayer;
  delete win()[KILL_SWITCH];
  win().__TAURI_INTERNALS__ = {};
  vi.stubEnv("DEV", false);
});

afterEach(() => {
  delete win().__TAURI_INTERNALS__;
  vi.unstubAllEnvs();
  vi.restoreAllMocks();
});

describe("initAnalytics", () => {
  it("does nothing before the onboarding wizard completes", async () => {
    const { initAnalytics } = await freshModule();
    initAnalytics();
    expect(injected).toEqual([]);
    expect(win().gtag).toBeUndefined();
  });

  it("loads once the wizard has completed", async () => {
    localStorage.setItem("wowsp-onboarding-completed", "1");
    const { initAnalytics } = await freshModule();
    initAnalytics();
    initAnalytics();
    expect(injected).toEqual(["https://www.googletagmanager.com/gtag/js?id=G-BGD43VPDEJ"]);
  });

  it("stays off when the user opted out", async () => {
    localStorage.setItem("wowsp-onboarding-completed", "1");
    localStorage.setItem("wowsp-analytics-disabled", "1");
    const { initAnalytics, isAnalyticsEnabled } = await freshModule();
    expect(isAnalyticsEnabled()).toBe(false);
    initAnalytics();
    expect(injected).toEqual([]);
  });

  it("stays off in dev builds", async () => {
    vi.stubEnv("DEV", true);
    localStorage.setItem("wowsp-onboarding-completed", "1");
    const { initAnalytics } = await freshModule();
    initAnalytics();
    expect(injected).toEqual([]);
  });
});

describe("setAnalyticsEnabled", () => {
  it("persists the opt-out and raises the gtag kill switch", async () => {
    localStorage.setItem("wowsp-onboarding-completed", "1");
    const { initAnalytics, setAnalyticsEnabled, trackPageView } = await freshModule();
    initAnalytics();
    const pushed = (win().dataLayer as unknown[]).length;

    setAnalyticsEnabled(false);
    expect(localStorage.getItem("wowsp-analytics-disabled")).toBe("1");
    expect(win()[KILL_SWITCH]).toBe(true);
    trackPageView("dashboard", "/dashboard");
    expect((win().dataLayer as unknown[]).length).toBe(pushed);
  });

  it("re-enabling clears the opt-out and loads the tag", async () => {
    localStorage.setItem("wowsp-onboarding-completed", "1");
    localStorage.setItem("wowsp-analytics-disabled", "1");
    const { setAnalyticsEnabled, isAnalyticsEnabled } = await freshModule();

    setAnalyticsEnabled(true);
    expect(isAnalyticsEnabled()).toBe(true);
    expect(localStorage.getItem("wowsp-analytics-disabled")).toBeNull();
    expect(win()[KILL_SWITCH]).toBe(false);
    expect(injected).toEqual(["https://www.googletagmanager.com/gtag/js?id=G-BGD43VPDEJ"]);
  });
});
