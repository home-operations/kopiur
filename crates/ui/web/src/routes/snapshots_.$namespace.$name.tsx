import { createFileRoute, redirect } from "@tanstack/react-router";

import { inspectToken } from "../components/inspect";
import { namespaceFromSearch } from "../util/namespace";

/**
 * A snapshot's old address. Its details live in the resource drawer now, so a
 * bookmark lands on the snapshots list with the drawer open on the same one.
 * The page's `?retention=open` has no meaning there and is dropped: the plan
 * is read when the drawer's Retention tab is opened.
 *
 * The file browser beneath this address (`…/browse`) is a page of its own and
 * is untouched: its file is the escaped `$name_`, so it is not this route's
 * child.
 */
export const Route = createFileRoute("/snapshots_/$namespace/$name")({
  beforeLoad: ({ params, search }) => {
    const scope = namespaceFromSearch(search);
    redirect({
      throw: true,
      to: "/snapshots",
      search: {
        ...(scope !== undefined ? { namespace: scope } : {}),
        inspect: inspectToken({ kind: "snapshot", ...params }),
      },
    });
  },
});
