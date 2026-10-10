import { useSyncExternalStore } from "react";

import { useBrowseSession } from "../../api/hooks";
import { parseSnapshotParam } from "./browse";
import { type BrowseStatus, browseStatus, browsed } from "./browsed";

/**
 * How Browse stands, for its line in the sidebar: grey with no session, green
 * while one runs on the snapshot Browse is on, red when the last start on it
 * failed. The session is the same read the page makes, so the two agree.
 */
export function useBrowseStatus(): BrowseStatus {
  const on = useSyncExternalStore(browsed.subscribe, browsed.get);
  const target = parseSnapshotParam(on.snapshot);
  const session = useBrowseSession(target?.namespace ?? "", target?.name ?? "", {
    enabled: target !== null,
  });
  return browseStatus(on, session.data != null, session.isError);
}
