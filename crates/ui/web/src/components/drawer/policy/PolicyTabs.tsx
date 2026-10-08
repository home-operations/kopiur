import { Link } from "@tanstack/react-router";
import { Camera, Database, FolderTree, ShieldCheck, Timer } from "lucide-react";

import type { PolicyDetail, SnapshotRow } from "../../../api/types";
import { EMPTY_CELL, humanBytes, relativeTime } from "../../../util/format";
import { Facts } from "../../Facts";
import { LampBadge } from "../../HealthBadge";
import { InspectLink } from "../../InspectLink";
import { KindChip } from "../../KindMark";
import { snapshotCount, retentionRules } from "../../policy";
import { LastSuccess } from "../../PolicyTable";
import { snapshotPhaseLamp } from "../../snapshot";
import { DrawerSection } from "../DrawerSection";

/** What it backs up, and where it writes. */
export function BacksUpTab({ detail, now }: { detail: PolicyDetail; now: Date }) {
  const { row } = detail;
  const scope = { namespace: row.namespace };
  return (
    <>
      <DrawerSection title="What it backs up" icon={FolderTree}>
        <Facts
          label="Sources"
          facts={[
            {
              term: "Kopia identity",
              value:
                detail.identity !== null && detail.identity !== undefined ? (
                  <span className="mono">{detail.identity}</span>
                ) : (
                  "not resolved yet — the policy has not been reconciled"
                ),
            },
            {
              term: "Sources",
              value:
                detail.sources.length === 0 ? (
                  "none — this policy backs up nothing"
                ) : (
                  <ul className="ref-list" aria-label="Sources">
                    {detail.sources.map((source) => (
                      <li key={source} className="label-strip">
                        <span className="label-strip__name">{source}</span>
                      </li>
                    ))}
                  </ul>
                ),
            },
          ]}
        />
        <p className="page__section-note">
          Paths from the last run when there is one, otherwise the sources as written.
        </p>
      </DrawerSection>

      <DrawerSection title="Where it writes" icon={Database}>
        <Facts
          label="Repositories"
          facts={[
            {
              term: "Repositories",
              value:
                row.repositories.length === 0 ? (
                  "none — this policy has nowhere to write"
                ) : (
                  <ul className="ref-list" aria-label="Repositories">
                    {row.repositories.map((repository) => (
                      <li key={repository} className="label-strip">
                        <span className="label-strip__name">{repository}</span>
                      </li>
                    ))}
                  </ul>
                ),
            },
            {
              term: "Fan-out",
              value: row.multiRepo
                ? "yes — one Snapshot per repository per run"
                : "no — one Snapshot per run",
            },
            { term: "Live snapshots", value: snapshotCount(row.activeSnapshotCount) },
            {
              term: "Last successful snapshot",
              value: <LastSuccess at={row.lastSuccessfulSnapshot} now={now} />,
            },
          ]}
        />
        <p className="page__section-note">
          <Link to="/repositories" search={scope}>
            Repositories
          </Link>{" "}
          shows each one&apos;s health, and whether anything is holding it back.
        </p>
      </DrawerSection>
    </>
  );
}

export function RetentionTab({ detail }: { detail: PolicyDetail }) {
  const rules = retentionRules(detail.retention);
  return (
    <DrawerSection title="Retention" icon={Timer}>
      {rules.length === 0 ? (
        <p className="page__section-note">
          No GFS retention is configured on this policy. Set{" "}
          <span className="mono">spec.retention</span> to control how long snapshots are kept.
        </p>
      ) : (
        <>
          <ul className="retention-rules" aria-label="Retention rules">
            {rules.map((rule) => (
              <li key={rule.field}>
                <span className="retention-rules__count">{rule.count}</span>
                <span className="retention-rules__term">{rule.term}</span>
                <span className="retention-rules__field mono">{rule.field}</span>
              </li>
            ))}
          </ul>
          <p className="page__section-note">
            A snapshot is kept if any rule still wants it. Pinned snapshots are always kept.
          </p>
        </>
      )}
    </DrawerSection>
  );
}

export function VerificationTab({ detail, now }: { detail: PolicyDetail; now: Date }) {
  return (
    <DrawerSection title="Verification" icon={ShieldCheck}>
      {detail.verification.length === 0 ? (
        <p className="page__section-note">
          Nothing to verify against: this policy names no repository.
        </p>
      ) : (
        // A ledger rather than `Facts`: the term here is a repository key,
        // and a `Facts` term is set in label caps — identifiers stay in
        // monospace and in their own case.
        <div className="ledger-scroll">
          <table className="ledger" aria-label="Verification">
            <thead>
              <tr>
                <th scope="col">Repository</th>
                <th scope="col" className="num">
                  Last verified
                </th>
              </tr>
            </thead>
            <tbody>
              {detail.verification.map((entry) => (
                <tr key={entry.repository}>
                  <td className="mono">{entry.repository}</td>
                  <td className="num">
                    <LastSuccess at={entry.lastVerified} now={now} never="never verified" />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <p className="page__section-note">
        Verification reads snapshots back to prove they can be restored.
      </p>
    </DrawerSection>
  );
}

export function SnapshotsTab({ detail, now }: { detail: PolicyDetail; now: Date }) {
  return (
    <DrawerSection title="Recent snapshots" icon={Camera}>
      <RecentSnapshots snapshots={detail.recentSnapshots} now={now} />
      <p className="page__section-note">
        The most recent runs this policy produced.{" "}
        <Link to="/snapshots" search={{ namespace: detail.row.namespace }}>
          Snapshots
        </Link>{" "}
        lists every one, with its size, retention and lineage.
      </p>
    </DrawerSection>
  );
}

/**
 * The runs this recipe produced, most recent first.
 *
 * A `Snapshot` has a phase rather than a lamp the operator published.
 * Rather than derive one here — and derive it a second way from the snapshot
 * screens — the phase is printed as the operator's own word, loud only when
 * it says `Failed`.
 */
function RecentSnapshots({ snapshots, now }: { snapshots: readonly SnapshotRow[]; now: Date }) {
  if (snapshots.length === 0) {
    return (
      <p className="page__section-note">
        This policy has produced no snapshots. A schedule fires it on a cron, and &ldquo;snapshot
        now&rdquo; below runs it once.
      </p>
    );
  }
  return (
    <div className="ledger-scroll">
      <table className="ledger" aria-label="Recent snapshots">
        <thead>
          <tr>
            <th scope="col">Snapshot</th>
            <th scope="col">Phase</th>
            <th scope="col">Repository</th>
            <th scope="col" className="num">
              Size
            </th>
            <th scope="col" className="num">
              Started
            </th>
          </tr>
        </thead>
        <tbody>
          {snapshots.map((snapshot) => (
            <tr key={`${snapshot.namespace}/${snapshot.name}`} data-kind="snapshot">
              <td className="has-stripe">
                <div className="table__object">
                  <KindChip kind="snapshot" size="sm" />
                  <InspectLink
                    className="row-link mono"
                    target={{
                      kind: "snapshot",
                      namespace: snapshot.namespace,
                      name: snapshot.name,
                    }}
                  >
                    {snapshot.name}
                  </InspectLink>
                </div>
              </td>
              <td>
                <SnapshotPhase phase={snapshot.phase} pinned={snapshot.pinned} />
              </td>
              <td className="mono">{snapshot.repository ?? EMPTY_CELL}</td>
              <td className="num">{humanBytes(snapshot.sizeBytes)}</td>
              <td className="num">
                {snapshot.startTime !== null && snapshot.startTime !== undefined ? (
                  <time dateTime={snapshot.startTime}>{relativeTime(snapshot.startTime, now)}</time>
                ) : (
                  EMPTY_CELL
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/**
 * A snapshot's phase, drawn the one way this console draws a phase.
 *
 * This used to narrow the union here and render the raw lowercase word,
 * colouring only `failed` — so the *same fact* was a lamp on `/snapshots` and
 * red prose here, and on this screen the failure was told by hue alone.
 * `snapshotPhaseLamp` is the single reading of the union (the `{ unknown:
 * { raw } }` arm included, so a phase a newer operator writes still appears,
 * under the unknown lamp rather than vanishing or reading as healthy), and
 * `LampBadge` is the single way to draw one.
 *
 * An absent phase is passed through too, and reads "Unreconciled" as it does
 * on `/snapshots`: the operator has written no phase at all, which is a thing
 * worth saying, and the em-dash this cell used to show read as "not
 * applicable".
 */
function SnapshotPhase({ phase, pinned }: { phase: SnapshotRow["phase"]; pinned: boolean }) {
  return (
    <>
      <LampBadge lamp={snapshotPhaseLamp(phase)} />
      {pinned ? <span className="policy-table__note"> pinned</span> : null}
    </>
  );
}
