import { useEffect, useState } from "react";
import { readSidebarCollapsed, writeSidebarCollapsed } from "./layout";
import {
  EpochCard,
  StatusCard,
  TransactionsCard,
  ValidatorsCard,
} from "./components/cards";
import { Header } from "./components/Header";
import { CachesCard } from "./components/CachesCard";
import { HostCard } from "./components/HostCard";
import { IngestCard } from "./components/IngestCard";
import { NetworkCard } from "./components/NetworkCard";
import { ReplayCard } from "./components/ReplayCard";
import { SchedulePage } from "./components/SchedulePage";
import { SlotDetailsPage } from "./components/SlotDetailsPage";
import { Sidebar } from "./components/Sidebar";
import { VersionsCard } from "./components/VersionsCard";
import { TpuPathCard } from "./components/TpuPathCard";
import { SlotStrip } from "./components/SlotStrip";
import { usePage, type Page } from "./route";
import { useStore } from "./useStore";

/** Base title, kept in step with index.html so the tab reads the same before
 *  the first snapshot arrives as it does after. */
const TITLE = "Agave Dashboard";

export function App() {
  const store = useStore();
  const connection = store.getConnection();
  const name = store.get<string | null>("summary", "identity_name");
  const identity = store.get<string>("summary", "identity_key");
  const [collapsed, setCollapsed] = useState(readSidebarCollapsed);
  const [page, setPage] = usePage();

  // Named after the validator so that an operator watching several at once can
  // tell the tabs apart. `Private` matches what the header shows for a node
  // with no on-chain name, and the plain title stands until a node answers.
  useEffect(() => {
    const label = name ?? (identity ? "Private" : null);
    document.title = label ? `${TITLE} | ${label}` : TITLE;
  }, [name, identity]);

  // Only the overview keeps the slot rail. The schedule lists the same slots in
  // more detail, so the rail beside it would be the same thing twice, and the
  // block detail wants the width more than it wants the context. The collapsed
  // state is remembered rather than reset, so coming back finds it as it was.
  const rail = page === "overview";
  const classes = ["app"];
  if (rail && collapsed) classes.push("is-collapsed");
  if (!rail) classes.push("is-full");

  return (
    <div className={classes.join(" ")}>
      {rail && (
        <Sidebar
          collapsed={collapsed}
          onToggle={() => {
            const next = !collapsed;
            setCollapsed(next);
            writeSidebarCollapsed(next);
          }}
        />
      )}
      <main className="main">
        <Header />
        {connection === "closed" && (
          <div className="banner">
            Disconnected from the validator. Retrying…
          </div>
        )}
        <Nav page={page} onSelect={setPage} />
        {page === "overview" && <Overview />}
        {page === "slots" && <SlotDetailsPage />}
        {page === "schedule" && <SchedulePage />}
      </main>
    </div>
  );
}

/** What this validator is doing, which is what the dashboard opens on. */
function Overview() {
  return (
    <>
      <SlotStrip />
      <div className="grid">
        <EpochCard />
        <StatusCard />
        <ValidatorsCard />
        <VersionsCard />
      </div>
      <TransactionsCard />
      {/* Both read the same traffic from opposite ends: bytes on the wire, and
          what the sockets failed to take off it. */}
      <div className="grid">
        <NetworkCard />
        <IngestCard />
      </div>
      {/* The machine the three cards above are running on. Last of the host
          group rather than first: an operator comes to this page for the
          validator, and reaches for the box only once something here says the
          validator is struggling. */}
      <HostCard />
      {/* What replay does, then the two things it spends that time waiting
          on. Loading programs and loading accounts are rows on the first card
          and sections of the one under it, so they read downwards: how long,
          then how well each of the two is going. */}
      <ReplayCard />
      <CachesCard />
      {/* Picks the same traffic up where the socket card leaves it. That one
          counts the datagrams the kernel never handed over; this one counts
          what the QUIC listener above it made of the rest, and carries the
          three QUIC ports the socket card therefore does not draw. */}
      <TpuPathCard />
    </>
  );
}

const PAGES: { page: Page; label: string }[] = [
  { page: "overview", label: "Overview" },
  { page: "slots", label: "Slot details" },
  { page: "schedule", label: "Schedule" },
];

/**
 * Anchors rather than buttons, so a page can be opened in a new tab and the
 * address bar says which one is on screen. The click is still handled, to keep
 * the switch to a re-render rather than a reload.
 */
function Nav({ page, onSelect }: { page: Page; onSelect: (page: Page) => void }) {
  return (
    <nav className="nav" aria-label="Pages">
      {PAGES.map((entry) => (
        <a
          key={entry.page}
          href={entry.page === "overview" ? "#" : `#/${entry.page}`}
          className={`nav-tab${page === entry.page ? " is-current" : ""}`}
          aria-current={page === entry.page ? "page" : undefined}
          onClick={(event) => {
            event.preventDefault();
            onSelect(entry.page);
          }}
        >
          {entry.label}
        </a>
      ))}
    </nav>
  );
}
