import { createFileRoute, redirect } from "@tanstack/react-router";

import { browseOffsetParam, browsePathParam, browseSearch } from "../components/browse/browse";
import { namespaceFromSearch } from "../util/namespace";

/**
 * A snapshot's old browse address. The browser is a section of its own now,
 * with the snapshot in its search, so a bookmark lands there with the same
 * snapshot, directory and page.
 */
export const Route = createFileRoute("/snapshots_/$namespace/$name_/browse")({
  beforeLoad: ({ params, search }) => {
    const raw: Record<string, unknown> = search;
    redirect({
      throw: true,
      to: "/browse",
      search: browseSearch(params.namespace, params.name, {
        scope: namespaceFromSearch(raw),
        path: browsePathParam(raw.path),
        offset: browseOffsetParam(raw.offset),
      }),
    });
  },
});
