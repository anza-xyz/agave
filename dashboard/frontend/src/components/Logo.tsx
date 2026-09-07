import { useState } from "react";

/**
 * A validator's on-chain icon.
 *
 * The URL is arbitrary and published by a third party, so it may be missing,
 * dead, or not an image. A failed load hides the element rather than leaving a
 * broken-image box, and `no-referrer` keeps the dashboard's own address out of
 * the request.
 */
export function Logo({ url, size }: { url: string | null; size: number }) {
  const [failed, setFailed] = useState(false);
  if (!url || failed) return null;
  return (
    <img
      className="logo"
      src={url}
      width={size}
      height={size}
      alt=""
      loading="lazy"
      decoding="async"
      referrerPolicy="no-referrer"
      onError={() => setFailed(true)}
    />
  );
}
