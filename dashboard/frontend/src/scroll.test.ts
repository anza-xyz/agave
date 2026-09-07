import { describe, expect, it } from "vitest";
import { heldScrollTop } from "./scroll";

describe("heldScrollTop", () => {
  it("moves down by whatever arrived above, so the view does not", () => {
    expect(heldScrollTop(900, 900, 5000, 5116)).toBe(1016);
  });

  it("leaves a viewer at the live edge alone", () => {
    // Being at the top is a request to see what arrives next.
    expect(heldScrollTop(0, 0, 5000, 5116)).toBe(0);
  });

  it("does nothing when the list did not grow", () => {
    expect(heldScrollTop(900, 900, 5000, 5000)).toBe(900);
  });

  it("does not drag the view when the list shrinks", () => {
    // Slots are pruned from the bottom, which moves nothing above them.
    expect(heldScrollTop(900, 900, 5000, 4800)).toBe(900);
  });

  it("stands aside when the browser already anchored", () => {
    // Where scroll anchoring fires it has already added the growth. Adding it
    // again would send the list twice as far as it should go.
    expect(heldScrollTop(1016, 900, 5000, 5116)).toBe(1016);
  });
});
