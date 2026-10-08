import { createFileRoute, redirect } from "@tanstack/react-router";

import { inspectToken } from "../components/inspect";
import { namespaceFromSearch } from "../util/namespace";

/**
 * A policy's old address. Its details live in the resource drawer now, so a
 * bookmark lands on the policies list with the drawer open on the same one.
 */
export const Route = createFileRoute("/policies_/$namespace/$name")({
  beforeLoad: ({ params, search }) => {
    const scope = namespaceFromSearch(search);
    throw redirect({
      to: "/policies",
      search: {
        ...(scope !== undefined ? { namespace: scope } : {}),
        inspect: inspectToken({ kind: "snapshotPolicy", ...params }),
      },
    });
  },
});
