import { describe, expect, it } from "vitest";
import {
  ceilingFor,
  columnRows,
  geometry,
  MATRIX_WINDOW_SECONDS,
  columnsFor,
  MIN_PITCH,
  ROWS_TALL,
  slotsFor,
} from "./matrix";

/** A mainnet-shaped second: vote, then failed, then succeeded on top. */
const MAINNET = [1654.28, 412, 1087.44];
const CEILING = ceilingFor(3684);

describe("columnRows", () => {
  it("stacks the series bottom up and fills the column to the total", () => {
    const lit = columnRows(MAINNET, CEILING, ROWS_TALL);
    expect(lit).toEqual([5, 1, 3]);
    // Nine of eleven rows lit, against a total that is 78% of the ceiling.
    expect(lit.reduce((sum, rows) => sum + rows, 0)).toBe(9);
  });

  it("never lights more rows than the grid has", () => {
    const lit = columnRows([5000, 5000, 5000], CEILING, ROWS_TALL);
    expect(lit.reduce((sum, rows) => sum + rows, 0)).toBeLessThanOrEqual(ROWS_TALL);
  });

  it("gives a series too small to round to a row one anyway", () => {
    // On this ceiling a row is worth about 368 tps, so 40 failed rounds to
    // nothing. An unlit band reads as no failures rather than few.
    const lit = columnRows([1654.28, 40, 1087.44], CEILING, ROWS_TALL);
    expect(lit[1]).toBe(1);
  });

  it("leaves a series at nought unlit", () => {
    // The guarantee is for small, not for absent. Nothing failed here.
    const lit = columnRows([1654.28, 0, 1087.44], CEILING, ROWS_TALL);
    expect(lit[1]).toBe(0);
  });

  it("takes the guaranteed row from the largest series, not from another small one", () => {
    // A column already full, with a sliver that has to fit somewhere.
    const lit = columnRows([4000, 1, 60], CEILING, ROWS_TALL);
    expect(lit[1]).toBe(1);
    expect(lit[2]).toBeGreaterThanOrEqual(1);
    expect(lit.reduce((sum, rows) => sum + rows, 0)).toBeLessThanOrEqual(ROWS_TALL);
  });

  it("lights nothing on an idle second", () => {
    expect(columnRows([0, 0, 0], CEILING, ROWS_TALL)).toEqual([0, 0, 0]);
  });

  it("divides by neither a ceiling nor a grid of nought", () => {
    expect(columnRows(MAINNET, 0, ROWS_TALL)).toEqual([0, 0, 0]);
    expect(columnRows(MAINNET, CEILING, 0)).toEqual([0, 0, 0]);
  });
});

describe("the grid's columns", () => {
  const samples = Array.from({ length: 60 }, (_unused, index) => index);

  it("gives every column at least the legible pitch", () => {
    const width = 340;
    expect(width / slotsFor(width)).toBeGreaterThanOrEqual(MIN_PITCH);
  });

  it("never has more columns than the window holds", () => {
    // A very wide card does not get a wider grid; a full minute fills it
    // exactly, and beyond that the dots would simply spread apart.
    expect(slotsFor(4000)).toBe(60);
  });

  it("drops columns rather than let one go below a legible width", () => {
    expect(slotsFor(340)).toBe(26);
    expect(slotsFor(340)).toBeLessThan(60);
  });

  it("keeps at least one column on a card too narrow for any", () => {
    expect(slotsFor(0)).toBe(1);
  });

  it("does not halve the grid when the window carries one sample too many", () => {
    // `windowed` keeps one sample past the left edge on purpose, so a full
    // minute arrives as sixty-one samples against sixty columns. Rounding the
    // stride up makes that a stride of two, and the grid visibly halves and
    // un-halves every time a sample lands.
    const over = Array.from({ length: 61 }, (_unused, index) => index);
    const columns = columnsFor(over, 60);
    expect(columns).toHaveLength(60);
    expect(columns.filter((column) => column === null)).toHaveLength(0);
    expect(columns[columns.length - 1]).toBe(60);
    expect(columns[0]).toBe(1);
  });

  it("thins to fit and keeps the newest sample last", () => {
    // Counted forward instead, the newest is dropped whenever the stride does
    // not divide evenly and the leading edge stops moving.
    const columns = columnsFor(samples, 26);
    expect(columns).toHaveLength(26);
    expect(columns[columns.length - 1]).toBe(59);
  });

  it("pads the left with nothing while the window is still filling", () => {
    // The unlit columns are what make a validator that has just started look
    // like a grid waiting to fill rather than a panel that has failed.
    const columns = columnsFor([1, 2, 3], 10);
    expect(columns).toHaveLength(10);
    expect(columns.slice(0, 7)).toEqual([null, null, null, null, null, null, null]);
    expect(columns.slice(7)).toEqual([1, 2, 3]);
  });

  it("returns a grid of nothing before any sample arrives", () => {
    expect(columnsFor([], 5)).toEqual([null, null, null, null, null]);
  });
});

describe("geometry", () => {
  it("keeps the dot square and leaves a gap on both axes", () => {
    const { dot, pitch, rowHeight } = geometry(780, 132, 60, ROWS_TALL);
    expect(dot).toBeLessThan(pitch);
    expect(dot).toBeLessThan(rowHeight);
    expect(dot).toBeGreaterThanOrEqual(3);
  });

  it("caps the dot so a wide card does not draw blocks", () => {
    expect(geometry(2000, 400, 20, ROWS_TALL).dot).toBe(6);
  });

  it("holds a floor so a narrow card draws dots rather than dust", () => {
    expect(geometry(120, 60, 60, ROWS_TALL).dot).toBe(3);
  });
});

describe("the scale", () => {
  it("sits above the window's peak, so the tallest column is not flush", () => {
    expect(ceilingFor(1000)).toBeCloseTo(1100, 6);
  });

  it("is never nought, so nothing divides by it before the first sample", () => {
    expect(ceilingFor(0)).toBe(1);
  });

  it("covers the same minute as the other charts", () => {
    expect(MATRIX_WINDOW_SECONDS).toBe(60);
  });
});
