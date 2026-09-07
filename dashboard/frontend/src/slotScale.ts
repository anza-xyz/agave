/**
 * Bar height for a slot duration, as a percentage.
 *
 * Scaled by ratio to nominal rather than linearly: a nominal slot sits at half
 * height, each doubling adds a quarter, each halving takes one away. A linear
 * scale saturated at twice nominal, which made a one-slot gap and a three-slot
 * gap the same bar.
 *
 * Its own module rather than a helper inside the component, so that the scale
 * can be tested without a DOM. This is the part worth testing: the saturation
 * bug above was invisible in review and obvious in a table of values.
 */
export function barHeight(durationMs: number | null, nominalMs: number): number {
  if (durationMs === null || durationMs <= 0) return 6;
  return Math.max(8, Math.min(100, 50 + 25 * Math.log2(durationMs / nominalMs)));
}
