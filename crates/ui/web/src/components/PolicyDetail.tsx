import { Link } from "@tanstack/react-router";
import {
  Camera,
  Database,
  FolderTree,
  ScrollText,
  ShieldAlert,
  ShieldCheck,
  Timer,
  type LucideIcon,
} from "lucide-react";
import type { ReactNode } from "react";

import type {
  ConditionView,
  PolicyDetail as PolicyDetailData,
  PolicyRow,
  RepositorySummary,
  SnapshotRow,
} from "../api/types";
import { EMPTY_CELL, humanBytes, relativeTime } from "../util/format";
import { DetailHeader, type TrailHop } from "./DetailHeader";
import { Facts } from "./Facts";
import { FlowLanes, type Lane, type LaneItem } from "./FlowLanes";
import { Finding } from "./Finding";
import { LampBadge } from "./HealthBadge";
import { KindChip } from "./KindMark";
import { LastSuccess } from "./PolicyTable";
import { gateSeverityLamp } from "./gates";
import { type Lamp, healthLamp } from "./health";
import { detailHref, parseRef } from "./kind";
import { policyVerdict, retentionRules, snapshotCount } from "./policy";
import { snapshotPhaseLamp } from "./snapshot";

/**
 * One policy, in the order an operator needs it.
 *
 * The verdict first — what this recipe writes and whether it has ever
 * worked. Then the gates, which are the reasons a human has to act before
 * anything else moves. Then the recipe itself: what it backs up, what it
 * keeps, how it is verified. Then the clocks that fire it and the runs it has
 * produced. Conditions last, as the evidence for everything above.
 *
 * Presentation only. Every mutating control is passed in by the route, which
 * owns the hooks — so this renders identically in a test with no query
 * client, exactly as `RepositoryDetail` does.
 */
export interface PolicyDetailProps {
  detail: PolicyDetailData;
  /** The snapshot-now and suspend controls, built by the route. */
  actions?: ReactNode;
  now?: Date | undefined;
  /** Repository rows, to show where it writes as cards; references otherwise. */
  repositoryRows?: readonly RepositorySummary[] | undefined;
}

/**
 * The hero's pill: suspended, then the worst gate holding it, else active.
 * "Never succeeded" and "never verified" are loud facts in the strip below,
 * not a pill — the same split the overview's policy tile makes.
 */
function policyLamp(detail: PolicyDetailData): Lamp {
  if (detail.row.suspended) return healthLamp("suspended");
  const gates = detail.gates.map((gate) => gateSeverityLamp(gate.severity));
  return (
    gates.find((lamp) => lamp.key === "failed") ??
    gates[0] ?? { ...healthLamp("healthy"), word: "Active" }
  );
}

/**
 * Where a policy sits: the repository it writes into. A fan-out has no single
 * upstream — two repositories in a trail would read as a chain — so it gets
 * no trail, and the Writes-into lane names them all.
 */
function policyTrail(row: PolicyRow): TrailHop[] {
  const only = row.repositories.length === 1 ? parseRef(row.repositories[0] ?? "") : null;
  if (only === null) return [];
  return [
    {
      kind: only.kind,
      name: only.name,
      namespace: only.namespace,
      to: detailHref(only.kind, only.name, only.namespace),
    },
  ];
}

/**
 * Fired by → this policy → writes into. A repository the server named with no
 * row loaded stays a reference; one whose key does not parse stays its text.
 */
function policyLanes(
  detail: PolicyDetailData,
  repositoryRows: readonly RepositorySummary[] | undefined,
): Lane[] {
  const { row } = detail;
  const writes: LaneItem[] = row.repositories.flatMap((key): LaneItem[] => {
    const ref = parseRef(key);
    if (ref === null) return [{ text: key }];
    const loaded = repositoryRows?.find(
      (r) =>
        r.name === ref.name &&
        (r.kind === "ClusterRepository") === (ref.kind === "clusterRepository") &&
        (r.namespace ?? undefined) === ref.namespace,
    );
    return [
      loaded !== undefined
        ? {
            card: {
              kind: ref.kind === "clusterRepository" ? "clusterRepository" : "repository",
              row: loaded,
            },
          }
        : { ref },
    ];
  });
  return [
    {
      title: "Fired by",
      label: "Schedules that fire this policy",
      items: detail.schedules.map((schedule) => ({
        card: { kind: "snapshotSchedule", row: schedule },
      })),
      empty: "No schedule fires this policy, so it only runs on request.",
    },
    {
      title: "This policy",
      label: "This policy",
      items: [{ card: { kind: "snapshotPolicy", row } }],
      empty: "",
      variant: "stats",
    },
    {
      title: "Writes into",
      label: "Writes into",
      items: writes,
      empty: "This policy names no repository, so it has nowhere to write.",
    },
  ];
}

export function PolicyDetail({
  detail,
  actions,
  now = new Date(),
  repositoryRows,
}: PolicyDetailProps) {
  const { row } = detail;
  const verdict = policyVerdict(row);
  const scope = { namespace: row.namespace };
  const rules = retentionRules(detail.retention);

  return (
    <div className="page">
      <DetailHeader
        kind="snapshotPolicy"
        name={row.name}
        namespace={row.namespace}
        lamp={policyLamp(detail)}
        verdictLabel="Policy verdict"
        verdict={verdict.text}
        trail={policyTrail(row)}
        actions={actions}
        stats={[
          { label: "Live snapshots", value: snapshotCount(row.activeSnapshotCount) },
          {
            label: "Last success",
            value:
              row.lastSuccessfulSnapshot !== null && row.lastSuccessfulSnapshot !== undefined
                ? relativeTime(row.lastSuccessfulSnapshot, now)
                : { absent: "loud", text: "never succeeded" },
            abs: row.lastSuccessfulSnapshot ?? undefined,
          },
          {
            label: "Last verified",
            value:
              row.lastVerified !== null && row.lastVerified !== undefined
                ? relativeTime(row.lastVerified, now)
                : { absent: "loud", text: "never verified" },
            abs: row.lastVerified ?? undefined,
          },
          { label: "Schedules", value: detail.schedules.length.toLocaleString() },
        ]}
      />

      <FlowLanes label="Relationships" lanes={policyLanes(detail, repositoryRows)} />

      {detail.gates.length > 0 ? (
        <Section title="Gates holding this policy" icon={ShieldAlert}>
          <ul className="finding-list">
            {detail.gates.map((gate) => {
              const gateLamp = gateSeverityLamp(gate.severity);
              return (
                <li key={`${gate.condition}/${gate.reason}`}>
                  <Finding
                    title={gate.reason}
                    what={gate.message}
                    lamp={gateLamp}
                    meta={
                      <span className="mono">
                        condition {gate.condition} · {gateLamp.word}
                      </span>
                    }
                  />
                </li>
              );
            })}
          </ul>
          <p className="page__section-note">
            A gate never self-heals — the policy stays parked until a human acts. The{" "}
            <Link to="/gates" search={scope}>
              gate registry
            </Link>{" "}
            explains every gate the operator can raise.
          </p>
        </Section>
      ) : null}

      <div className="page__pair">
        <Section title="What it backs up" icon={FolderTree}>
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
        </Section>

        <Section title="Where it writes" icon={Database}>
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
        </Section>
      </div>

      <div className="page__pair">
        <Section title="Retention" icon={Timer}>
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
        </Section>

        <Section title="Verification" icon={ShieldCheck}>
          {detail.verification.length === 0 ? (
            <p className="page__section-note">
              Nothing to verify against: this policy names no repository.
            </p>
          ) : (
            // A ledger rather than `Facts`: the term here is a repository
            // key, and a `Facts` term is set in label caps —
            // `Repository/media/nas` would be shown as
            // `REPOSITORY/MEDIA/NAS`, which is not the object's name.
            // Identifiers stay in monospace and in their own case.
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
        </Section>
      </div>

      <Section title="Recent snapshots" icon={Camera}>
        <RecentSnapshots snapshots={detail.recentSnapshots} now={now} />
        <p className="page__section-note">
          The most recent runs this policy produced.{" "}
          <Link to="/snapshots" search={scope}>
            Snapshots
          </Link>{" "}
          lists every one, with its size, retention and lineage.
        </p>
      </Section>

      <Section title="Conditions" icon={ScrollText}>
        <Conditions conditions={detail.conditions} now={now} />
      </Section>
    </div>
  );
}

interface SectionProps {
  title: string;
  icon: LucideIcon;
  children: ReactNode;
}

function Section({ title, icon: Icon, children }: SectionProps) {
  return (
    <section className="page__section" aria-label={title}>
      <div className="page__section-head">
        <h2>
          <Icon size={16} strokeWidth={2} aria-hidden="true" />
          {title}
        </h2>
      </div>
      {children}
    </section>
  );
}

/**
 * The runs this recipe produced, most recent first.
 *
 * Deliberately not a `WorkTable`: that ledger requires a `Health` per row,
 * and a `Snapshot` has a phase rather than a lamp the operator published.
 * Rather than derive one here — and derive it a second way from the snapshot
 * screens — the phase is printed as the operator's own word, loud only when
 * it says `Failed`.
 */
function RecentSnapshots({ snapshots, now }: { snapshots: readonly SnapshotRow[]; now: Date }) {
  if (snapshots.length === 0) {
    return (
      <p className="page__section-note">
        This policy has produced no snapshots. A schedule fires it on a cron, and &ldquo;snapshot
        now&rdquo; above runs it once.
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
                  <Link
                    className="mono"
                    to="/snapshots/$namespace/$name"
                    params={{ namespace: snapshot.namespace, name: snapshot.name }}
                  >
                    {snapshot.name}
                  </Link>
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

function Conditions({ conditions, now }: { conditions: readonly ConditionView[]; now: Date }) {
  if (conditions.length === 0) {
    return (
      <p className="page__section-note">
        No conditions yet: the operator has not reconciled this policy.
      </p>
    );
  }
  return (
    <div className="ledger-scroll">
      <table className="ledger" aria-label="Conditions">
        <thead>
          <tr>
            <th scope="col">Type</th>
            <th scope="col">Status</th>
            <th scope="col">Reason</th>
            <th scope="col">Message</th>
            <th scope="col" className="num">
              Since
            </th>
          </tr>
        </thead>
        <tbody>
          {conditions.map((condition) => (
            <tr key={condition.type}>
              <td className="mono">{condition.type}</td>
              <td className="mono">{condition.status}</td>
              <td className="mono">{condition.reason ?? EMPTY_CELL}</td>
              <td>{condition.message ?? EMPTY_CELL}</td>
              <td className="num">
                {condition.lastTransitionTime !== null &&
                condition.lastTransitionTime !== undefined ? (
                  <time dateTime={condition.lastTransitionTime}>
                    {relativeTime(condition.lastTransitionTime, now)}
                  </time>
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
