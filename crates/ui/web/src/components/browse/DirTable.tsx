import { Link } from "@tanstack/react-router";
import { File, FileQuestion, Folder, Link2, type LucideIcon } from "lucide-react";

import type { DirEntryView, DirListing, EntryKind } from "../../api/types";
import { EMPTY_CELL, formatTimestamp, humanBytes } from "../../util/format";
import { ColumnLedger } from "../ColumnLedger";
import type { ColumnSpec } from "../tableColumns";
import { DownloadButton } from "./DownloadButton";
import { childPath, entryAction, entryKindLabel } from "./browse";

/**
 * One page of a directory inside the snapshot, as a ledger.
 *
 * The order is kopia's own manifest order, untouched. Sorting here would be
 * a lie across pages: the server paginates over that order, so a column sort
 * applied to 500 of 12,000 entries would reorder a window and imply it
 * reordered the directory.
 *
 * Every entry is listed, whatever it is. A symlink, a socket and a file too
 * large for a browser download all keep their row, their size and their kind
 * — only the control at the end changes, to a disabled one carrying the
 * reason. A row that vanished would read as "this backup does not contain
 * that", which is the one thing this screen exists to answer.
 *
 * Sizes: a file's is its own; a directory's is its subtree total, which is
 * what kopia records in `summ` and the server folds into `size`. An absent
 * size is the empty cell, never `0 B`.
 */
export interface DirTableProps {
  namespace: string;
  name: string;
  listing: DirListing;
  /** The console's `?namespace=` scope, carried into each directory link. */
  scope: string | undefined;
}

type DirColumn = "entry" | "kind" | "size" | "modified" | "mode" | "file";

const DIR_COLUMNS: readonly ColumnSpec<DirColumn>[] = [
  { id: "entry", label: "Entry", width: "auto", min: 240, locked: true },
  { id: "kind", label: "Kind", width: 110, min: 70 },
  { id: "size", label: "Size", width: 110, min: 96, numeric: true },
  { id: "modified", label: "Modified", width: 200, min: 110 },
  { id: "mode", label: "Mode", width: 120, min: 80, className: "mono" },
  { id: "file", label: "File", width: 220, min: 160, locked: true, resizable: false },
];

export function DirTable({ namespace, name, listing, scope }: DirTableProps) {
  const scoped = scope !== undefined ? { namespace: scope } : {};
  const caption =
    listing.path.length > 0 ? `Entries in ${listing.path}` : "Entries in the snapshot";
  return (
    <ColumnLedger
      id="browse"
      label={caption}
      className="dir-table"
      columns={DIR_COLUMNS}
      rows={listing.entries}
      rowKey={(entry) => childPath(listing.path, entry.name)}
      cell={(entry, id) => {
        const path = childPath(listing.path, entry.name);
        const action = entryAction(entry, listing.session);
        switch (id) {
          case "entry":
            return (
              <span className="dir-table__entry">
                <EntryIcon kind={entry.kind} />
                {action.action === "open" ? (
                  <Link
                    className="mono dir-table__name"
                    to="/snapshots/$namespace/$name/browse"
                    params={{ namespace, name }}
                    search={{ ...scoped, path }}
                  >
                    {entry.name}
                  </Link>
                ) : (
                  <span className="mono dir-table__name">{entry.name}</span>
                )}
              </span>
            );
          case "kind":
            return entryKindLabel(entry.kind);
          case "size":
            return humanBytes(entry.size);
          case "modified":
            return formatTimestamp(entry.mtime);
          case "mode":
            return modeCell(entry);
          case "file":
            return action.action === "open" ? (
              <span className="dir-table__note">Open it to list its contents.</span>
            ) : (
              <DownloadButton
                namespace={namespace}
                name={name}
                path={path}
                label={entry.name}
                disabledReason={action.action === "refused" ? action.reason : undefined}
                disabledWord={action.action === "refused" ? action.word : undefined}
              />
            );
          default:
            return id satisfies never;
        }
      }}
    />
  );
}

function modeCell(entry: DirEntryView): string {
  const mode = entry.mode;
  return mode !== null && mode !== undefined && mode.length > 0 ? mode : EMPTY_CELL;
}

/**
 * The kind's mark beside the name. Decorative only — `aria-hidden`, with the
 * Kind column saying the same thing in a word, because an icon is never the
 * only copy of a fact on this console.
 */
function EntryIcon({ kind }: { kind: EntryKind }) {
  if (typeof kind !== "string") {
    return <FileQuestion {...ICON} />;
  }
  switch (kind) {
    case "dir":
      return <Folder {...ICON} />;
    case "file":
      return <File {...ICON} />;
    case "symlink":
      return <Link2 {...ICON} />;
    default:
      return <FileQuestion {...ICON} />;
  }
}

/**
 * One spread rather than a `const Icon = lookup(kind)`: choosing a component
 * inside render is what `react-hooks/static-components` forbids, and each arm
 * returning its own element keeps the switch exhaustive over `EntryKind`.
 */
const ICON: Partial<Parameters<LucideIcon>[0]> = {
  className: "dir-table__icon",
  size: 14,
  strokeWidth: 2,
  "aria-hidden": true,
};
