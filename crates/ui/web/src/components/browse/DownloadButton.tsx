import { Download } from "lucide-react";

import { paths } from "../../api/hooks";
import { ActionButton } from "../ActionButton";

/**
 * Download one file out of the snapshot — **an anchor, never a `fetch`**.
 *
 * The whole point of this control is what it is *not*. `fetch`ing the file
 * would buffer every byte in the tab's heap before anything could be saved,
 * so a multi-gigabyte restore would take the page down. A plain `<a href>` is
 * a top-level navigation: the browser streams the response to disk, which is
 * also why the server serves `…/file` outside its request timeout and behind
 * `require_same_site_navigation` rather than the SPA's CSRF header.
 *
 * **No `download` attribute.** On a same-origin link its value *overrides*
 * `Content-Disposition`, and that header is where the server's filename
 * sanitisation lives (`content_disposition` keeps only the basename, strips
 * control characters and percent-encodes the rest). Naming the file from this
 * side would hand the browser an entry name the snapshot supplied, unchecked.
 *
 * A refusal is the same control, disabled with its reason, as a `button`:
 * the size gates are decided from `SessionInfo` before the click (addenda
 * item 20), because the `413`/`422` the server would answer with lands on a
 * navigation the SPA can never observe. Never hidden — a file too large for a
 * browser download is a fact about the backup worth reading.
 */
export interface DownloadButtonProps {
  namespace: string;
  name: string;
  /** The file's path inside the snapshot, relative to its root. */
  path: string;
  /** The file's own name, for the control's accessible name. */
  label: string;
  /** When set, the control is disabled and this is the full sentence. */
  disabledReason?: string | undefined;
  /**
   * The short form of that sentence, rendered in the cell beside the control.
   *
   * Required whenever `disabledReason` is set, and not decoration: a ledger
   * scrolls, a scroll container clips the floating tooltip a disabled button
   * carries, and `styles.css` therefore suppresses it inside `.ledger-scroll`
   * outright. Without this word a sighted mouse user would see a dead control
   * and no reason at all.
   */
  disabledWord?: string | undefined;
}

export function DownloadButton({
  namespace,
  name,
  path,
  label,
  disabledReason,
  disabledWord,
}: DownloadButtonProps) {
  // Both states take the default button shape, and that is deliberate. On the
  // quiet variant the *enabled* control was a bare underlined word while the
  // refused ones carried the disabled fill — so the dead controls out-shouted
  // the live one, which is backwards. Same box, two states: surface and a
  // strong hairline when it works, the inset and disabled ink when it does
  // not. The anchor keeps the console's link underline, which is the one
  // honest difference between them: this one navigates.
  if (disabledReason !== undefined && disabledReason.length > 0) {
    return (
      <div className="dir-table__file">
        <ActionButton
          className="dir-table__download"
          disabledReason={disabledReason}
          title={disabledReason}
        >
          <Download size={14} strokeWidth={2} aria-hidden="true" />
          <span className="visually-hidden">Download {label}</span>
          <span aria-hidden="true">Download</span>
        </ActionButton>
        {disabledWord !== undefined && disabledWord.length > 0 ? (
          <span className="dir-table__note" aria-hidden="true">
            {disabledWord}
          </span>
        ) : null}
      </div>
    );
  }
  return (
    <a className="button dir-table__download" href={paths.snapshotFile(namespace, name, path)}>
      <Download size={14} strokeWidth={2} aria-hidden="true" />
      <span className="visually-hidden">Download {label}</span>
      <span aria-hidden="true">Download</span>
    </a>
  );
}
