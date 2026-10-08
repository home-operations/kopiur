import { Link } from "@tanstack/react-router";
import { FolderOpen } from "lucide-react";

import { useCurrentNamespace } from "../../../util/namespace";

/**
 * Open the snapshot's file browser — a page of its own, because listing a
 * tree and downloading from it is a workspace, not a detail. The console's
 * scope rides along so the page is still narrowed to where the reader was.
 */
export function BrowseFiles({ namespace, name }: { namespace: string; name: string }) {
  const scope = useCurrentNamespace();
  return (
    <Link
      className="button"
      to="/snapshots/$namespace/$name/browse"
      params={{ namespace, name }}
      search={scope !== undefined ? { namespace: scope } : {}}
    >
      <FolderOpen size={14} strokeWidth={2} aria-hidden="true" />
      Browse files
    </Link>
  );
}
