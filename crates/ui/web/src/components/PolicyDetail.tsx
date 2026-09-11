import { Link } from "@tanstack/react-router";
import {
  CalendarClock,
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

import type { ConditionView, PolicyDetail as PolicyDetailData, SnapshotRow } from "../api/types";
import { EMPTY_CELL, humanBytes, relativeTime } from "../util/format";
import { Facts } from "./Facts";
import { Finding } from "./Finding";
import { LampBadge } from "./HealthBadge";
import { LastSuccess } from "./PolicyTable";
import { gateSeverityLamp } from "./gates";
import { healthLamp } from "./health";
import { policyVerdict, retentionRules, snapshotCount } from "./policy";
import { scheduleCron, scheduleFires, firesBySelector } from "./schedule";

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
}

export function PolicyDetail({ detail, actions, now = new Date() }: PolicyDetailProps) {
  const { row } = detail;
  const verdict = policyVerdict(row);
  const lamp = verdict.suspended ? healthLamp("suspended") : undefined;
  const scope = { namespace: row.namespace };
  const rules = retentionRules(detail.retention);

  return (
    <div className="page">
      <p className="verdict" role="status" aria-label="Policy verdict">
        {lamp !== undefined ? (
          <span className="verdict__lamp" data-health={lamp.key}>
            <lamp.icon size={18} strokeWidth={2} aria-hidden="true" />
            <span>{lamp.word}</span>
          </span>
        ) : null}
        <span className="verdict__text">{verdict.text}</span>
        <span className="verdict__meta mono">
          SnapshotPolicy · {row.namespace} · {row.name}
        </span>
      </p>

      {actions !== undefined && actions !== null ? (
        <section className="page__section" aria-label="Actions">
          <div className="action-bar">{actions}</div>
        </section>
      ) : null}

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
            The paths shown are the ones the last run actually resolved where there has been one,
            and the sources as written otherwise — a <span className="mono">pvcSelector</span> is
            shown as the selector rather than as the claims it happens to match today.
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
                  ? "yes — every run mints one Snapshot per repository"
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
              No GFS retention is configured on this policy. It states no rule about how long old
              snapshots are kept, so the fate of this policy&apos;s snapshots is not decided here at
              all — set <span className="mono">spec.retention</span> to decide it here. Read nothing
              further into the blank: an unstated rule is not a rule to discard, and it is not a
              rule to keep.
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
                Grandfather-father-son: a snapshot survives if it fills a slot any of these rules
                still wants. A pinned snapshot is exempt from all of them.
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
            <Facts
              label="Verification"
              facts={detail.verification.map((entry) => ({
                term: entry.repository,
                value: <LastSuccess at={entry.lastVerified} now={now} never="never verified" />,
              }))}
            />
          )}
          <p className="page__section-note">
            Verification reads the snapshot back out of the repository. A backup that has never been
            verified is a backup nobody has proved is restorable.
          </p>
        </Section>
      </div>

      <Section title="Schedules that fire this policy" icon={CalendarClock}>
        {detail.schedules.length === 0 ? (
          <p className="page__section-note">
            No schedule fires this policy, so it only runs when someone asks it to. A{" "}
            <span className="mono">SnapshotSchedule</span> either names a policy directly or selects
            one by label;{" "}
            <Link to="/schedules" search={scope}>
              Schedules
            </Link>{" "}
            lists every one in scope.
          </p>
        ) : (
          <>
            <div className="ledger-scroll">
              <table className="ledger" aria-label="Schedules">
                <thead>
                  <tr>
                    <th scope="col">Schedule</th>
                    <th scope="col">Cron</th>
                    <th scope="col">Fires</th>
                    <th scope="col">State</th>
                    <th scope="col" className="num">
                      Next
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {detail.schedules.map((schedule) => (
                    <tr key={`${schedule.namespace}/${schedule.name}`}>
                      <td className="mono">{schedule.name}</td>
                      <td className="mono">{scheduleCron(schedule)}</td>
                      <td>
                        {firesBySelector(schedule) ? (
                          <>
                            <span className="mono">{scheduleFires(schedule)}</span>{" "}
                            <span className="policy-table__note">by selector</span>
                          </>
                        ) : (
                          <span className="mono">{scheduleFires(schedule)}</span>
                        )}
                      </td>
                      <td>
                        {schedule.suspended ? (
                          <LampBadge lamp={healthLamp("suspended")} />
                        ) : (
                          <span className="policy-table__active">Active</span>
                        )}
                      </td>
                      <td className="num">
                        {schedule.nextFire !== null && schedule.nextFire !== undefined ? (
                          <time dateTime={schedule.nextFire}>
                            {relativeTime(schedule.nextFire, now)}
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
            <p className="page__section-note">
              This list is exact rather than approximate: a{" "}
              <span className="mono">policySelector</span> is evaluated with the operator&apos;s own
              matcher, <span className="mono">matchExpressions</span> included, so a schedule shown
              here really would fire this policy.
            </p>
          </>
        )}
      </Section>

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
            <tr key={`${snapshot.namespace}/${snapshot.name}`}>
              <td className="mono">{snapshot.name}</td>
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
 * A snapshot's phase as the operator's own word.
 *
 * The union is heterogeneous: the object arm is `{ unknown: { raw } }` and
 * renders that raw string, so a phase a newer operator writes appears rather
 * than vanishing. No narrowing needed beyond that — the word is the word.
 */
function SnapshotPhase({ phase, pinned }: { phase: SnapshotRow["phase"]; pinned: boolean }) {
  const word =
    phase === null || phase === undefined
      ? EMPTY_CELL
      : typeof phase === "string"
        ? phase
        : phase.unknown.raw;
  return (
    <>
      <span className={word === "failed" ? "policy-table__never" : undefined}>{word}</span>
      {pinned ? <span className="policy-table__note"> pinned</span> : null}
    </>
  );
}

function Conditions({ conditions, now }: { conditions: readonly ConditionView[]; now: Date }) {
  if (conditions.length === 0) {
    return (
      <p className="page__section-note">
        The operator has written no conditions on this policy yet, which means it has not been
        reconciled — not that it is healthy.
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
