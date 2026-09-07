import { useEffect, useState, type ReactNode } from "react";

/**
 * Text that copies itself to the clipboard when clicked.
 *
 * A button rather than a styled span so it is reachable by keyboard, and it
 * shrinks with an ellipsis rather than forcing its container wider, so a long
 * value shows in full when there is room and truncates when there is not.
 *
 * The confirmation is drawn over the value rather than replacing it. Swapping a
 * forty-four character address for the word "copied" collapsed the button's
 * width, and in a header that wraps, that relaid out everything after it — so
 * the act of copying an address made the page jump under the pointer.
 */
export function Copyable({
  text,
  label,
  className,
}: {
  text: string;
  /**
   * What to show, when that differs from what to copy. A slot number reads
   * better grouped and pastes better without the separators.
   */
  label?: ReactNode;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!copied) return;
    const timer = window.setTimeout(() => setCopied(false), 1200);
    return () => window.clearTimeout(timer);
  }, [copied]);

  const copy = async () => {
    if (await writeToClipboard(text)) setCopied(true);
  };

  return (
    <button
      type="button"
      className={`copyable${copied ? " copied" : ""}${className ? ` ${className}` : ""}`}
      onClick={copy}
      // Named explicitly because the value is hidden while the confirmation
      // shows, which would otherwise leave the button briefly nameless.
      aria-label={text}
      title={copied ? "Copied" : `${text}\nClick to copy`}
    >
      <span className="copyable-text">{label ?? text}</span>
      <span className="copyable-flash" aria-hidden="true">
        copied
      </span>
    </button>
  );
}

/**
 * `navigator.clipboard` only exists in a secure context, and the dashboard is
 * commonly served over plain HTTP on a private address, so fall back to the
 * deprecated selection-based copy when it is missing.
 */
async function writeToClipboard(text: string): Promise<boolean> {
  if (navigator.clipboard && window.isSecureContext) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // Fall through; a permission failure is still worth retrying the old way.
    }
  }

  const staging = document.createElement("textarea");
  staging.value = text;
  // Kept out of view and out of the tab order while it is briefly focused.
  staging.setAttribute("readonly", "");
  staging.style.position = "fixed";
  staging.style.top = "-1000px";
  staging.style.opacity = "0";
  document.body.appendChild(staging);
  staging.select();
  let copied = false;
  try {
    copied = document.execCommand("copy");
  } catch {
    copied = false;
  }
  document.body.removeChild(staging);
  return copied;
}
