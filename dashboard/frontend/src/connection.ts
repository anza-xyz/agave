/**
 * Keeps a websocket to the validator open, reconnecting with backoff.
 *
 * The server sends a full snapshot on connect, so a reconnect needs no catch-up
 * logic. Whatever arrives overwrites the store.
 */

import type { Store } from "./store";
import type { Envelope } from "./types";

// Timers are used unqualified rather than through `window`, which is the same
// function in a browser and lets this module be tested without one.
const MIN_RETRY_MS = 500;
const MAX_RETRY_MS = 10_000;

/**
 * How long the page waits for anything at all before treating the connection
 * as dead.
 *
 * A socket can stop delivering without ever closing — a NAT table dropping the
 * flow, a VPN reconnecting, a laptop waking up. The browser leaves it `OPEN`,
 * no event fires, and nothing here would ever notice: the page goes on showing
 * the last values it received, which look like live ones. The charts empty a
 * minute later because their window slides past the newest sample, while the
 * figures beside them stay frozen and plausible.
 *
 * Silence is a sound signal because the validator publishes its clock every
 * second whether or not anything else changed, so a working connection is never
 * quiet for long. Eight of those rather than two, because a mobile handover can
 * stall a connection for several seconds and the cost of being wrong is a
 * reconnect that pulls the whole snapshot down again.
 */
const SILENCE_LIMIT_MS = 8_000;

/** How often the silence is checked. */
const WATCHDOG_INTERVAL_MS = 2_000;

export function connect(store: Store): () => void {
  let socket: WebSocket | null = null;
  let retryMs = MIN_RETRY_MS;
  let timer: number | null = null;
  let watchdog: number | null = null;
  let lastMessageAt = Date.now();
  let closed = false;

  const url = () => {
    const protocol = location.protocol === "https:" ? "wss:" : "ws:";
    return `${protocol}//${location.host}/websocket`;
  };

  const stopWatchdog = () => {
    if (watchdog === null) return;
    clearInterval(watchdog);
    watchdog = null;
  };

  /**
   * Gives up on a socket that has gone quiet and starts another.
   *
   * Handlers are detached and the reconnect scheduled here rather than left to
   * `onclose`. Closing a socket whose peer is unreachable need not produce a
   * close event promptly — that is the same unreachability being worked around
   * — so waiting for one risks never reconnecting at all.
   */
  const abandon = () => {
    stopWatchdog();
    const dead = socket;
    socket = null;
    if (dead) {
      dead.onopen = null;
      dead.onmessage = null;
      dead.onclose = null;
      dead.onerror = null;
      try {
        dead.close();
      } catch {
        // Already gone. Nothing here depends on it closing cleanly.
      }
    }
    store.setConnection("closed");
    schedule();
  };

  const open = () => {
    if (closed) return;
    store.setConnection("connecting");
    const ws = new WebSocket(url());
    socket = ws;
    lastMessageAt = Date.now();

    // Every handler checks it is still the current socket. An abandoned one can
    // fire late, and it must not disturb the connection that replaced it.
    ws.onopen = () => {
      if (ws !== socket) return;
      retryMs = MIN_RETRY_MS;
      lastMessageAt = Date.now();
      // Installed before the state changes, so a caller that reacts to the
      // connection opening can send straight away.
      store.setSender((frame) => ws.send(frame));
      store.setConnection("open");
      stopWatchdog();
      watchdog = setInterval(() => {
        // Wall clock rather than a monotonic one, so that a device waking from
        // sleep counts the time it was away and reconnects at once.
        if (Date.now() - lastMessageAt >= SILENCE_LIMIT_MS) abandon();
      }, WATCHDOG_INTERVAL_MS);
    };

    ws.onmessage = (event) => {
      if (ws !== socket) return;
      // Recorded before the frame is understood: anything arriving proves the
      // connection is delivering, which is all this is watching for.
      lastMessageAt = Date.now();
      if (typeof event.data !== "string") return;
      let envelope: Envelope;
      try {
        envelope = JSON.parse(event.data) as Envelope;
      } catch {
        // A malformed frame is a server bug. Dropping it beats tearing down a
        // connection that is otherwise working.
        return;
      }
      store.apply(envelope);
    };

    ws.onclose = () => {
      if (ws !== socket) return;
      stopWatchdog();
      store.setConnection("closed");
      schedule();
    };

    ws.onerror = () => {
      if (ws !== socket) return;
      ws.close();
    };
  };

  const schedule = () => {
    if (closed || timer !== null) return;
    timer = setTimeout(() => {
      timer = null;
      open();
    }, retryMs);
    retryMs = Math.min(retryMs * 2, MAX_RETRY_MS);
  };

  open();

  return () => {
    closed = true;
    stopWatchdog();
    if (timer !== null) clearTimeout(timer);
    socket?.close();
  };
}
