import { memo, useRef, useState } from "react";
import { ScrollTop } from "./ScrollTop";
import { count, shortKey } from "../format";
import type { LeaderRef } from "../schedule";
import type { SlotEntry } from "../types";
import { useStore } from "../useStore";
import { Logo } from "./Logo";

/**
 * Rows in the live slot list, newest first.
 *
 * Everything the client holds. The server sends 512 slots on connect and the
 * store keeps that many, so a smaller figure here threw away rows that had
 * already been paid for over the wire.
 */
const ROWS = 512;

/**
 * The live slot list, with a filter down to this validator's own leader slots.
 *
 * The filter is local to the sidebar on purpose. The strip in the Slots panel
 * is a picture of what the cluster is doing and stays whole whichever view is
 * chosen here.
 */
export function Sidebar({
  collapsed,
  onToggle,
}: {
  collapsed: boolean;
  onToggle: () => void;
}) {
  const store = useStore();
  const [ownOnly, setOwnOnly] = useState(false);
  // Newest first, so the live edge of the list is the top of it.
  const rows = useRef<HTMLDivElement>(null);
  const all = store.getSlots();
  const slots = (ownOnly ? all.filter((entry) => entry.mine) : all)
    .slice(-ROWS)
    .reverse();

  const action = collapsed ? "Show the slot list" : "Hide the slot list";

  return (
    <aside className="sidebar">
      <div className="sidebar-head">
        {/* Stays put when everything else goes, or there would be nothing left
            to bring the list back with. */}
        <button
          type="button"
          className="sidebar-collapse"
          onClick={onToggle}
          aria-expanded={!collapsed}
          aria-controls="slot-list"
          title={action}
          aria-label={action}
        >
          <Chevron collapsed={collapsed} />
        </button>
        {!collapsed && (
          <>
            <span>Slots</span>
            <div className="sidebar-filter" role="group" aria-label="Which slots to list">
              <button type="button" aria-pressed={!ownOnly} onClick={() => setOwnOnly(false)}>
                All
              </button>
              <button type="button" aria-pressed={ownOnly} onClick={() => setOwnOnly(true)}>
                Ours
              </button>
            </div>
          </>
        )}
      </div>
      {!collapsed && (
        <div className="sidebar-rows" id="slot-list" ref={rows}>
          <ScrollTop scroller={rows} />
          {slots.length === 0 && (
            <div className="sidebar-empty">
              {ownOnly ? "no leader slots seen yet" : "waiting for slots…"}
            </div>
          )}
          {slots.map((entry) => (
            <SidebarRow
              key={entry.slot}
              entry={entry}
              leader={store.leaderOf(entry.slot, entry.mine)}
            />
          ))}
        </div>
      )}
    </aside>
  );
}

/** Points the way the list will move: back in when collapsed, away when open. */
function Chevron({ collapsed }: { collapsed: boolean }) {
  return (
    <svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true">
      <path
        d={collapsed ? "M6 3l5 5-5 5" : "M10 3L5 8l5 5"}
        fill="none"
        stroke="currentColor"
        strokeWidth="1.7"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/**
 * Memoised because the list is long and the store is chatty: it notifies on
 * every published value, including the once-a-second meter samples that touch
 * no slot at all. The store replaces only the entries that changed, so identity
 * holds for the rest and five hundred untouched rows are skipped rather than
 * diffed.
 */
const SidebarRow = memo(function SidebarRow({
  entry,
  leader,
}: {
  entry: SlotEntry;
  leader: LeaderRef;
}) {
  const name = leader.name ?? (leader.key ? shortKey(leader.key, 4, 4) : "unknown");

  return (
    <div className={`sidebar-row level-${entry.level}${entry.mine ? " mine" : ""}`}>
      <div className="sidebar-leader" title={leader.key ?? undefined}>
        {entry.mine && <span className="sidebar-mine-marker" aria-label="our slot" />}
        <Logo url={leader.icon} size={14} />
        {name}
      </div>
      <div className="sidebar-slot">{count(entry.slot)}</div>
      <div className={`sidebar-level level-${entry.level}`} title={entry.level.replace(/_/g, " ")} />
    </div>
  );
});
