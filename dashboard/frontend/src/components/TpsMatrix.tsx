import { useRef } from "react";
import {
  ceilingFor,
  columnRows,
  geometry,
  MATRIX_WINDOW_SECONDS,
  columnsFor,
  ROWS_SHORT,
  ROWS_TALL,
  slotsFor,
} from "../matrix";
import type { TpsSample } from "../types";
import { RENDER_LAG_MS, useNow, windowed } from "../useNow";
import { useWidth } from "../useWidth";

/** The three series, bottom of the column upwards. */
const SERIES = ["vote", "failed", "success"] as const;

/**
 * A minute of throughput as a grid of lit dots, one column per sample.
 *
 * Each column is lit from the bottom: vote first, then the non-vote traffic
 * that failed, then the non-vote traffic that succeeded on top. So the height
 * of the lit part reads as total throughput and the three colours read as the
 * split, which the stacked areas this replaces never managed. Nothing on that
 * chart said which band was which.
 *
 * Failures sit between the two rather than on top because they are the band
 * that changes most and the eye finds a moving band more easily against a
 * still one above and below it.
 *
 * Drawn as one path per colour rather than one element per dot. Sixty columns
 * of eleven rows is six hundred and sixty squares, and six paths reconcile in
 * a fraction of the time six hundred elements do, while staying SVG: the
 * colours still come from the stylesheet and both themes still work.
 *
 * The scale is fixed at a tenth above the window's peak rather than fitted to
 * each frame, so the silhouette does not rescale every time a spike arrives
 * and leaves.
 */
export function TpsMatrix({ samples, short }: { samples: TpsSample[]; short?: boolean }) {
  const box = useRef<HTMLDivElement>(null);
  const width = useWidth(box);
  // Drawn a sample behind live, so the newest column is complete rather than
  // arriving mid-second.
  const edge = useNow() - RENDER_LAG_MS;

  const rows = short ? ROWS_SHORT : ROWS_TALL;
  const height = rows * (short ? 9 : 12);
  const windowMs = MATRIX_WINDOW_SECONDS * 1000;
  const visible = windowed(samples, edge, windowMs, (sample) => sample.timestamp_nanos);

  return (
    <div className="matrix" ref={box} style={{ height }}>
      {width === null ? null : (
        <Grid samples={visible} width={width} height={height} rows={rows} />
      )}
    </div>
  );
}

function Grid({
  samples,
  width,
  height,
  rows,
}: {
  samples: TpsSample[];
  width: number;
  height: number;
  rows: number;
}) {
  const columns = columnsFor(samples, slotsFor(width));
  const peak = Math.max(...samples.map((sample) => sample.total), 0);
  const ceiling = ceilingFor(peak);
  const { pitch, rowHeight, dot } = geometry(width, height, columns.length, rows);

  // One path per colour. The live column takes the brighter set, so the leading
  // edge reads without a marker of its own.
  const paths = new Map<string, string[]>();
  const add = (key: string, x: number, y: number) => {
    const square = `M${x.toFixed(2)} ${y.toFixed(2)}h${dot.toFixed(2)}v${dot.toFixed(2)}h-${dot.toFixed(2)}Z`;
    const held = paths.get(key);
    if (held) held.push(square);
    else paths.set(key, [square]);
  };

  columns.forEach((sample, index) => {
    const live = index === columns.length - 1;
    const x = index * pitch + pitch / 2 - dot / 2;
    // A column with nothing behind it is drawn as an unlit one, which is what
    // makes a validator that has just started look like a grid waiting to fill
    // rather than a panel that has failed.
    const lit = sample
      ? columnRows([sample.vote, sample.non_vote_failed, sample.non_vote_success], ceiling, rows)
      : [0, 0, 0];

    let row = 0;
    for (const [series, count] of lit.entries()) {
      for (let step = 0; step < count; step += 1, row += 1) {
        const y = height - (row + 0.5) * rowHeight - dot / 2;
        add(`${SERIES[series]}${live ? "-live" : ""}`, x, y);
      }
    }
    // The unlit rows are drawn, not left out. The dark grid above the lit part
    // is what says how much headroom there is.
    for (; row < rows; row += 1) {
      const y = height - (row + 0.5) * rowHeight - dot / 2;
      add("off", x, y);
    }
  });

  return (
    <svg width={width} height={height} viewBox={`0 0 ${width} ${height}`} role="img">
      {/* Unlit first, so nothing is drawn over a lit dot. */}
      {["off", ...SERIES, ...SERIES.map((series) => `${series}-live`)].map((key) => {
        const squares = paths.get(key);
        return squares ? <path key={key} className={`matrix-${key}`} d={squares.join("")} /> : null;
      })}
    </svg>
  );
}
