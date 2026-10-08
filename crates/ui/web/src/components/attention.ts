import type {
  DoctorCheckView,
  DoctorObjectView,
  Health,
  MaintenanceRow,
  PolicyRow,
  RepositorySummary,
  ScheduleRow,
} from "../api/types";
import type { StalledRowView } from "../api/statusReport";
import type { InspectTarget } from "./inspect";
import { kindOfLabel } from "./kind";
import { repositoryPhaseLabel } from "./repository";

/**
 * What the overview's "Needs attention" lists: one row per object that needs
 * someone, whichever read noticed it, and one line per failing doctor check
 * that is not about any object in particular.
 *
 * Three reads can say the same object is in trouble: the doctor names it, the
 * status report lists it as stalled, and its kind's list shows it unhealthy.
 * Shown side by side they told the same story three times in three styles, so
 * each object gets one row and the most specific account of it wins — the
 * doctor's (the operator's diagnosis plus the check's fix), then the status
 * report's condition message, then what the list row alone can say.
 */

/** At most this many rows; the kind lists and the doctor report have the rest. */
export const ATTENTION_MAX = 8;

/** One thing wrong with an object, and what to do about it when someone said. */
export interface AttentionProblem {
  what: string;
  fix?: string;
}

export interface AttentionItem {
  target: InspectTarget;
  health: Health;
  /** The pill's word: what is wrong, not the object's own phase. */
  state: string;
  problems: AttentionProblem[];
  /** RFC3339 instant it went wrong, when a source dated it. */
  at?: string;
}

/** A failing doctor check about the installation rather than an object. */
export interface AttentionCheck {
  check: string;
  title: string;
  what: string;
  fix?: string;
}

export interface Attention {
  items: AttentionItem[];
  /** Rows beyond {@link ATTENTION_MAX}, not shown. */
  more: number;
  checks: AttentionCheck[];
}

export interface AttentionSources {
  repositories?: readonly RepositorySummary[] | undefined;
  policies?: readonly PolicyRow[] | undefined;
  schedules?: readonly ScheduleRow[] | undefined;
  maintenance?: readonly MaintenanceRow[] | undefined;
  stalled?: readonly StalledRowView[] | undefined;
  checks?: readonly DoctorCheckView[] | undefined;
}

/**
 * The operator writes its remedy into the condition message after `Fix:`; the
 * row gives it the fix plate instead of leaving it mid-sentence. A message
 * without one is all "what".
 */
export function splitFix(message: string): AttentionProblem {
  const match = /(^|[\s.;])Fix: /.exec(message);
  if (match === null) return { what: message };
  const at = match.index + (match[1] ?? "").length;
  const what = message.slice(0, at).trim();
  const fix = message.slice(at + "Fix: ".length).trim();
  return what.length > 0 && fix.length > 0 ? { what, fix } : { what: message };
}

/** Which account of an object wins: lower is more specific. */
const Source = { Doctor: 0, Stalled: 1, List: 2 } as const;
type Source = (typeof Source)[keyof typeof Source];

/** Sort order: failures, then warnings, then what is merely not yet known. */
function rankOf(health: Health): number {
  switch (health) {
    case "failed":
      return 0;
    case "degraded":
      return 1;
    case "unknown":
    case "pending":
      return 3;
    case "healthy":
    case "suspended":
      return 4;
  }
}

/** A policy that was never verified is worth knowing, after everything else. */
const NEVER_VERIFIED_RANK = 2;

/** The pill's word for an object a doctor check named. */
function doctorState(check: string): string {
  switch (check) {
    case "no-stuck-work":
      return "Stuck";
    case "recent-failures":
      return "Failed";
    case "repositories-ready":
      return "Not ready";
    default:
      return "Failing";
  }
}

interface Entry {
  item: AttentionItem;
  source: Source;
  rank: number;
  order: number;
}

function key(target: InspectTarget): string {
  return `${target.kind}/${target.namespace ?? ""}/${target.name}`;
}

function plural(n: number, word: string): string {
  return `${String(n)} ${word}${n === 1 ? "" : "s"}`;
}

/** `namespace/name` → a target of `kind`; a namespaced kind must have both. */
function stalledTarget(row: StalledRowView): InspectTarget | null {
  const kind = kindOfLabel(row.kind);
  if (kind === null) return null;
  const slash = row.object.indexOf("/");
  if (kind === "clusterRepository") {
    return slash < 0 ? { kind, name: row.object } : null;
  }
  if (slash <= 0 || slash === row.object.length - 1) return null;
  return { kind, namespace: row.object.slice(0, slash), name: row.object.slice(slash + 1) };
}

function doctorTarget(object: DoctorObjectView): InspectTarget {
  return object.kind === "clusterRepository"
    ? { kind: object.kind, name: object.name }
    : { kind: object.kind, name: object.name, namespace: object.namespace ?? undefined };
}

function repositoryTarget(r: RepositorySummary): InspectTarget {
  return r.kind === "ClusterRepository"
    ? { kind: "clusterRepository", name: r.name }
    : { kind: "repository", name: r.name, namespace: r.namespace ?? undefined };
}

export function attention(sources: AttentionSources): Attention {
  const entries = new Map<string, Entry>();
  const add = (
    source: Source,
    target: InspectTarget,
    health: Health,
    state: string,
    problem: AttentionProblem,
    extra: { rank?: number; at?: string | null | undefined } = {},
  ) => {
    const k = key(target);
    const existing = entries.get(k);
    const rank = extra.rank ?? rankOf(health);
    if (existing !== undefined && existing.source < source) return;
    if (existing?.source === source) {
      if (!existing.item.problems.some((p) => p.what === problem.what)) {
        existing.item.problems.push(problem);
      }
      if (rank < existing.rank) {
        existing.rank = rank;
        existing.item.health = health;
        existing.item.state = state;
      }
      return;
    }
    const item: AttentionItem = { target, health, state, problems: [problem] };
    if (extra.at !== undefined && extra.at !== null) item.at = extra.at;
    entries.set(k, { item, source, rank, order: existing?.order ?? entries.size });
  };

  const checks: AttentionCheck[] = [];
  for (const check of sources.checks ?? []) {
    if (check.outcome !== "Fail") continue;
    const failing = check.objects.filter((o) => o.failing);
    if (failing.length === 0) {
      const line: AttentionCheck = {
        check: check.check,
        title: check.title,
        what: check.what ?? "The check failed without saying what it found.",
      };
      if (check.fix !== undefined && check.fix !== null) line.fix = check.fix;
      checks.push(line);
      continue;
    }
    for (const object of failing) {
      const problem = splitFix(object.message);
      if (problem.fix === undefined && object.fix !== undefined && object.fix !== null) {
        problem.fix = object.fix;
      }
      add(Source.Doctor, doctorTarget(object), "failed", doctorState(check.check), problem, {
        at: object.at,
      });
    }
  }

  for (const row of sources.stalled ?? []) {
    const target = stalledTarget(row);
    if (target === null) continue;
    const problem =
      row.message !== null && row.message.length > 0
        ? splitFix(row.message)
        : { what: "Stalled; the operator did not say why." };
    add(Source.Stalled, target, "failed", "Stalled", problem);
  }

  for (const r of sources.repositories ?? []) {
    if (r.health !== "failed" && r.health !== "degraded" && r.health !== "unknown") continue;
    const state =
      r.health === "failed" ? "Failed" : r.health === "degraded" ? "Degraded" : "Unknown";
    const what =
      r.phase !== undefined && r.phase !== null
        ? `The repository reports phase ${repositoryPhaseLabel(r.phase)}.`
        : "The repository has not reported a status yet.";
    add(Source.List, repositoryTarget(r), r.health, state, { what });
  }
  for (const s of sources.schedules ?? []) {
    if (s.suspended || s.consecutiveFailures <= 0) continue;
    add(
      Source.List,
      { kind: "snapshotSchedule", namespace: s.namespace, name: s.name },
      "failed",
      "Failing",
      { what: `${plural(s.consecutiveFailures, "run")} in a row failed.` },
    );
  }
  for (const m of sources.maintenance ?? []) {
    const quick = m.quick.consecutiveFailures;
    const full = m.full.consecutiveFailures;
    if (quick + full <= 0) continue;
    const what =
      quick > 0 && full > 0
        ? `${String(quick)} quick and ${String(full)} full maintenance runs in a row failed.`
        : quick > 0
          ? `${plural(quick, "quick maintenance run")} in a row failed.`
          : `${plural(full, "full maintenance run")} in a row failed.`;
    add(
      Source.List,
      { kind: "maintenance", namespace: m.namespace, name: m.name },
      "failed",
      "Failing",
      { what },
    );
  }
  for (const p of sources.policies ?? []) {
    if (p.suspended) continue;
    const target: InspectTarget = { kind: "snapshotPolicy", namespace: p.namespace, name: p.name };
    if (!p.lastSuccessfulSnapshot) {
      add(Source.List, target, "degraded", "Never succeeded", {
        what: "No snapshot of this policy has succeeded yet.",
      });
    } else if (!p.lastVerified) {
      add(
        Source.List,
        target,
        "degraded",
        "Never verified",
        { what: "None of this policy's snapshots has been verified yet." },
        { rank: NEVER_VERIFIED_RANK },
      );
    }
  }

  const sorted = [...entries.values()]
    .sort((a, b) => a.rank - b.rank || a.order - b.order)
    .map((e) => e.item);
  return {
    items: sorted.slice(0, ATTENTION_MAX),
    more: Math.max(0, sorted.length - ATTENTION_MAX),
    checks,
  };
}
