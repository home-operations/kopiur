import { createFileRoute, redirect } from "@tanstack/react-router";

import { inspectToken } from "../components/inspect";
import { namespaceFromSearch } from "../util/namespace";

/**
 * A restore's old address. Its details live in the resource drawer now, so a
 * bookmark lands on the restores list with the drawer open on the same one.
 */
export const Route = createFileRoute("/restores_/$namespace/$name")({
  beforeLoad: ({ params, search }) => {
    const scope = namespaceFromSearch(search);
    redirect({
      throw: true,
      to: "/restores",
      search: {
        ...(scope !== undefined ? { namespace: scope } : {}),
        inspect: inspectToken({ kind: "restore", ...params }),
      },
    });
  },
});
