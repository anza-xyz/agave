import { describe, expect, it } from "vitest";
import { pageHash, readPage } from "./route";

describe("readPage", () => {
  it("reads a page out of the hash, with or without the slash", () => {
    expect(readPage("#/schedule")).toBe("schedule");
    expect(readPage("#schedule")).toBe("schedule");
    expect(readPage("#/slots")).toBe("slots");
  });

  it("falls back to the overview rather than showing nothing", () => {
    // A hash is anyone's to type, and a blank page is a worse answer than the
    // page they started on.
    expect(readPage("")).toBe("overview");
    expect(readPage("#")).toBe("overview");
    expect(readPage("#/nonsense")).toBe("overview");
  });
});

describe("pageHash", () => {
  it("round-trips every page", () => {
    for (const page of ["overview", "slots", "schedule"] as const) {
      expect(readPage(pageHash(page))).toBe(page);
    }
  });

  it("clears the hash for the overview instead of naming it", () => {
    expect(pageHash("overview")).toBe("#");
  });
});
