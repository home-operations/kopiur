import type { BrowseStatus } from "../browse/browsed";

/**
 * The line itself: one segment, like a section whose objects share one
 * health, in the same colours — grey idle, green active, red failed. It is
 * `data-browse`, not `data-health`: it is decoration over the link's
 * description, not a lamp that must carry its own icon and word.
 */
export function BrowseLine({ status }: { status: BrowseStatus }) {
  return (
    <span className="status-bar status-bar--nav" aria-hidden="true">
      <span data-browse={status.state} style={{ flexGrow: 1 }} />
    </span>
  );
}
