import { Link } from "@tanstack/react-router";

import type { Lineage } from "../api/types";

/**
 * Where this snapshot came from and what has been copied out of it — the
 * replication trail, read left to right.
 *
 * The trail is deliberately **one hop up and one hop down**, because that is
 * all the correlation can honestly carry. A copy records the *source manifest
 * id*, and `snapshot migrate` mints a new id in the destination, so the server
 * can match `copiedFrom.sourceManifestId == this.kopiaSnapshotId` in one
 * direction only (`view_lineage`). Walking further would mean guessing.
 *
 * The upstream hop is a **repository and a manifest id, not a link**: the
 * source `Snapshot` resource lives in whatever cluster wrote it and may not
 * exist here at all. Naming it without linking is honest; a link to a route
 * that cannot resolve is not (the reference-list rule).
 *
 * Downstream copies are real `Snapshot` resources this caller listed, so those
 * are links.
 */
export interface LineageTrailProps {
  lineage: Lineage;
  /** The snapshot the trail is drawn around. */
  name: string;
  namespace: string;
}

export function LineageTrail({ lineage, name, namespace }: LineageTrailProps) {
  const from = lineage.copiedFromRepository;
  const manifest = lineage.sourceManifestId;
  const hasSource = from !== null && from !== undefined && from.length > 0;
  const { copies } = lineage;

  if (!hasSource && copies.length === 0) {
    return (
      <p className="page__section-note">
        This snapshot was written where it stands. It is not a copy of another repository&apos;s
        snapshot, and nothing has been replicated out of it — a{" "}
        <span className="mono">SnapshotReplication</span> is what would create a copy, and each copy
        would appear here as its own <span className="mono">Snapshot</span> resource.
      </p>
    );
  }

  return (
    <>
      <ol className="lineage" aria-label="Replication lineage">
        {hasSource ? (
          <li className="lineage__step" data-step="source">
            <span className="label-strip">
              <span className="label-strip__kind">from repository</span>
              <span className="label-strip__name">{from}</span>
            </span>
            {manifest !== null && manifest !== undefined && manifest.length > 0 ? (
              <span className="lineage__meta mono">source manifest {manifest}</span>
            ) : null}
            <span className="lineage__note">
              Named, not linked: the source snapshot resource lives with the repository it was
              written in, which may be another cluster.
            </span>
          </li>
        ) : null}

        <li className="lineage__step" data-step="subject" aria-current="step">
          <span className="label-strip">
            <span className="label-strip__kind">this snapshot</span>
            <span className="label-strip__name mono">{name}</span>
          </span>
          <span className="lineage__meta mono">{namespace}</span>
        </li>

        {copies.map((copy) => (
          <li className="lineage__step" data-step="copy" key={`${copy.namespace}/${copy.name}`}>
            <span className="label-strip">
              <span className="label-strip__kind">copied to</span>
              <span className="label-strip__name">
                <Link
                  className="mono"
                  to="/snapshots/$namespace/$name"
                  params={{ namespace: copy.namespace, name: copy.name }}
                >
                  {copy.name}
                </Link>
              </span>
            </span>
            <span className="lineage__meta mono">{copy.namespace}</span>
          </li>
        ))}
      </ol>
      {copies.length === 0 ? (
        <p className="page__section-note">
          Nothing has been replicated out of this snapshot yet — no{" "}
          <span className="mono">Snapshot</span> this caller can see records it as a source.
        </p>
      ) : null}
    </>
  );
}
