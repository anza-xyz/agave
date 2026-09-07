import { count, percent, release, solCompact } from "../format";
import type { VersionShare } from "../types";
import { useStore } from "../useStore";
import { Card } from "./primitives";

/**
 * How the cluster's stake divides across client versions.
 *
 * Ordered and measured by stake rather than by node count: during an upgrade
 * what matters is how much of the vote has moved, not how many nodes are
 * carrying it. The counts beside each row are staked validators, the same
 * population the validator card counts, so the two cards add up to each other.
 */
export function VersionsCard() {
  const store = useStore();
  const shares = store.get<VersionShare[]>("summary", "versions");
  // Rows are keyed by release, so a build of 4.3.0-beta.0 belongs to the 4.3.0
  // row and has to be shortened to find it.
  const ours = release(store.get<string>("summary", "version"));

  if (!shares || shares.length === 0) {
    return <Card title="Cluster Versions">waiting for data…</Card>;
  }

  const totalStake = shares.reduce((sum, share) => sum + share.stake, 0);

  return (
    <Card title="Cluster Versions">
      <div className="versions">
        {shares.map((share, index) => {
          const fraction = totalStake === 0 ? 0 : share.stake / totalStake;
          // A folded tail and a group reporting no version both have no
          // version, so the server flags which is which rather than leaving it
          // to be guessed from ordering.
          const label = share.other ? "other" : (share.version ?? "unknown");
          const isOurs = share.version !== null && share.version === ours;

          return (
            <div className={`version-row${isOurs ? " is-ours" : ""}`} key={`${label}-${index}`}>
              <div className="version-name">
                {label}
                {isOurs && <span className="version-ours">ours</span>}
              </div>
              <div className="version-bar">
                <i style={{ width: `${Math.min(100, fraction * 100)}%` }} />
              </div>
              <div className="version-share">{percent(fraction, 1)}</div>
              <div className="version-count">
                {count(share.validators)} validators · {solCompact(share.stake)} SOL
              </div>
            </div>
          );
        })}
      </div>
    </Card>
  );
}
