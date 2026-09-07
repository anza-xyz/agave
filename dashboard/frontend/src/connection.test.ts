import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { connect } from "./connection";
import { Store } from "./store";

/** Stands in for the browser's WebSocket, with the peer under test control. */
class FakeSocket {
  static live: FakeSocket[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  closed = false;

  constructor(public url: string) {
    FakeSocket.live.push(this);
  }

  /** The browser calling close() fires onclose; a dead peer may not. */
  close() {
    this.closed = true;
  }

  accept() {
    this.onopen?.();
  }

  deliver(topic: string, key: string, value: unknown) {
    this.onmessage?.({ data: JSON.stringify({ topic, key, value }) });
  }
}

const sockets = () => FakeSocket.live;
const latest = () => FakeSocket.live[FakeSocket.live.length - 1];

beforeEach(() => {
  vi.useFakeTimers();
  FakeSocket.live = [];
  vi.stubGlobal("WebSocket", FakeSocket);
  vi.stubGlobal("location", { protocol: "http:", host: "validator:10999" });
  // The store notifies on an animation frame, which node has no concept of.
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
    cb(0);
    return 0;
  });
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("the silence watchdog", () => {
  it("reconnects a socket that stops delivering without ever closing", () => {
    // The failure this exists for: a NAT table drops the flow, the browser
    // leaves the socket OPEN, no event fires, and the page shows the values it
    // last received as though they were current.
    const store = new Store();
    connect(store);
    latest().accept();
    expect(store.getConnection()).toBe("open");

    latest().deliver("summary", "server_time_nanos", 1);
    vi.advanceTimersByTime(6_000);
    expect(sockets()).toHaveLength(1);
    expect(store.getConnection()).toBe("open");

    // Past the limit with nothing arriving, and the socket is abandoned.
    // Stepped to just after the watchdog and before the first retry, or the
    // reconnect below would already have happened and hidden this.
    vi.advanceTimersByTime(2_200);
    expect(store.getConnection()).toBe("closed");
    expect(sockets()[0].closed).toBe(true);
    expect(sockets()).toHaveLength(1);

    // The existing backoff then opens another.
    vi.advanceTimersByTime(500);
    expect(sockets()).toHaveLength(2);
  });

  it("stays connected while anything at all keeps arriving", () => {
    // The validator publishes its clock every second whether or not anything
    // else changed, so a working connection is never quiet for long.
    const store = new Store();
    connect(store);
    latest().accept();

    for (let second = 0; second < 30; second += 1) {
      vi.advanceTimersByTime(1_000);
      latest().deliver("summary", "server_time_nanos", second);
    }

    expect(sockets()).toHaveLength(1);
    expect(store.getConnection()).toBe("open");
  });

  it("tolerates a stall shorter than the limit", () => {
    // A mobile handover pauses a connection for a few seconds without it being
    // dead. Reconnecting through those would pull the whole snapshot each time.
    const store = new Store();
    connect(store);
    latest().accept();

    vi.advanceTimersByTime(6_000);
    latest().deliver("summary", "server_time_nanos", 1);
    vi.advanceTimersByTime(6_000);
    latest().deliver("summary", "server_time_nanos", 2);

    expect(sockets()).toHaveLength(1);
    expect(store.getConnection()).toBe("open");
  });

  it("counts a frame it cannot parse as proof of life", () => {
    // The connection is what is being watched, not the payload. A frame the
    // store rejects still shows the path is delivering.
    const store = new Store();
    connect(store);
    latest().accept();

    for (let i = 0; i < 4; i += 1) {
      vi.advanceTimersByTime(4_000);
      latest().onmessage?.({ data: "{ not json" });
    }

    expect(sockets()).toHaveLength(1);
    expect(store.getConnection()).toBe("open");
  });

  it("does not fire on a socket that has not opened yet", () => {
    // Before the handshake completes there is nothing to be silent, and the
    // close path already covers a connection that never comes up.
    const store = new Store();
    connect(store);
    vi.advanceTimersByTime(30_000);
    expect(sockets()).toHaveLength(1);
  });

  it("stops watching once the page is done with the connection", () => {
    // The teardown returned by connect must leave no timer running, or a test
    // suite and a closed tab both keep reopening sockets.
    const store = new Store();
    const stop = connect(store);
    latest().accept();
    stop();

    vi.advanceTimersByTime(60_000);
    expect(sockets()).toHaveLength(1);
  });

  it("ignores a late event from a socket it already gave up on", () => {
    // An abandoned socket can still fire, and it must not tear down the
    // connection that replaced it.
    const store = new Store();
    connect(store);
    latest().accept();

    vi.advanceTimersByTime(10_000);
    vi.advanceTimersByTime(600);
    expect(sockets()).toHaveLength(2);
    const replacement = latest();
    replacement.accept();

    sockets()[0].onclose?.();
    expect(store.getConnection()).toBe("open");
    expect(latest()).toBe(replacement);
  });
});
