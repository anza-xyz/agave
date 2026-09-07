import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  readSidebarCollapsed,
  SIDEBAR_STORAGE_KEY,
  writeSidebarCollapsed,
} from "./layout";

function storage(): Storage {
  const values = new Map<string, string>();
  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => void values.set(key, value),
    removeItem: (key: string) => void values.delete(key),
    clear: () => values.clear(),
    key: () => null,
    length: 0,
  } as unknown as Storage;
}

beforeEach(() => {
  vi.stubGlobal("window", { localStorage: storage() });
});

describe("sidebar collapse", () => {
  it("starts expanded when nothing has been chosen", () => {
    expect(readSidebarCollapsed()).toBe(false);
  });

  it("remembers the choice both ways", () => {
    writeSidebarCollapsed(true);
    expect(readSidebarCollapsed()).toBe(true);
    writeSidebarCollapsed(false);
    expect(readSidebarCollapsed()).toBe(false);
  });

  it("treats an unrecognised value as expanded", () => {
    window.localStorage.setItem(SIDEBAR_STORAGE_KEY, "yes");
    expect(readSidebarCollapsed()).toBe(false);
  });

  it("survives storage being refused", () => {
    // Private browsing and some embedded webviews throw on access rather than
    // returning null, which would otherwise take the whole app down at render.
    vi.stubGlobal("window", {
      get localStorage(): Storage {
        throw new Error("denied");
      },
    });
    expect(() => writeSidebarCollapsed(true)).not.toThrow();
    expect(readSidebarCollapsed()).toBe(false);
  });
});
