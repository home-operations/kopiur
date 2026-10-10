import { Link } from "@tanstack/react-router";
import { FolderOpen } from "lucide-react";

import { useCurrentNamespace } from "../../../util/namespace";
import { browseSearch } from "../../browse/browse";

/**
 * Open this snapshot in Browse — a section of its own, because listing a
 * tree and downloading from it is a workspace, not a detail — with the
 * snapshot already chosen. The console's scope rides along so the page is
 * still narrowed to where the reader was.
 */
export function BrowseFiles({ namespace, name }: { namespace: string; name: string }) {
  const scope = useCurrentNamespace();
  return (
    <Link className="button" to="/browse" search={browseSearch(namespace, name, { scope })}>
      <FolderOpen size={14} strokeWidth={2} aria-hidden="true" />
      Browse files
    </Link>
  );
}
