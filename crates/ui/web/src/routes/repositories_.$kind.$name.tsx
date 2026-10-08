import { createFileRoute, redirect } from "@tanstack/react-router";

import { type InspectTarget, inspectToken } from "../components/inspect";
import { namespaceFromSearch } from "../util/namespace";

/**
 * A repository's old address, `/repositories/<kindPath>/<name>[?namespace=]`.
 * Its details live in the resource drawer now, so a bookmark lands on the
 * repositories list with the drawer open on the same one. An address that
 * cannot name exactly one object — an unknown kind segment, or a namespaced
 * Repository with no namespace — lands on the list with nothing open.
 */
function target(kind: string, name: string, namespace: string | undefined): InspectTarget | null {
  switch (kind) {
    case "cluster-repository":
      return { kind: "clusterRepository", name };
    case "repository":
      return namespace !== undefined ? { kind: "repository", name, namespace } : null;
    default:
      return null;
  }
}

export const Route = createFileRoute("/repositories_/$kind/$name")({
  beforeLoad: ({ params, search }) => {
    const scope = namespaceFromSearch(search);
    const open = target(params.kind, params.name, scope);
    redirect({
      throw: true,
      to: "/repositories",
      search: {
        ...(scope !== undefined ? { namespace: scope } : {}),
        ...(open !== null ? { inspect: inspectToken(open) } : {}),
      },
    });
  },
});
