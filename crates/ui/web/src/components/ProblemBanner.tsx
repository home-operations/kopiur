import { CircleAlert, ShieldOff, X } from "lucide-react";
import { useSyncExternalStore } from "react";

import { isNotPermitted, problemBanner, problemKind } from "../api/problem";
import type { Problem } from "../api/types";

/**
 * A problem, rendered as what / why / fix with the fix set apart.
 *
 * The three lines are the server's own (or the client's synthetic ones); the
 * banner never paraphrases them. `fix` sits on its own plate with a label, so
 * the eye lands on the next step first when the page is already on fire.
 */
export interface ProblemBannerProps {
  problem: Problem;
  /** What the user was doing when it failed: "Suspend policy nightly". */
  source?: string | undefined;
  /** `banner` spans the shell under the header; `inline` sits in content. */
  variant?: "banner" | "inline" | undefined;
  onDismiss?: (() => void) | undefined;
}

export function ProblemBanner({
  problem,
  source,
  variant = "inline",
  onDismiss,
}: ProblemBannerProps) {
  const forbidden = isNotPermitted(problem);
  const Icon = forbidden ? ShieldOff : CircleAlert;
  const kind = problemKind(problem);
  const classes = [
    "problem",
    variant === "banner" ? "problem--banner" : "",
    forbidden ? "problem--forbidden" : "",
  ]
    .filter((c) => c.length > 0)
    .join(" ");
  return (
    <div className={classes} role="alert">
      <span className="problem__icon">
        <Icon size={18} strokeWidth={2} aria-hidden="true" />
      </span>
      <div className="problem__body">
        <div className="problem__what">{problem.what}</div>
        <div className="problem__why">{problem.why}</div>
        <div className="problem__fix">
          <span className="problem__fix-label">Fix</span>
          <span>{problem.fix}</span>
        </div>
        <div className="problem__meta">
          {source !== undefined ? <span className="problem__source">{source} · </span> : null}
          {problem.status > 0 ? `${problem.status} ` : ""}
          {kind ?? problem.title}
          {problem.kubeReason ? ` · ${problem.kubeReason}` : ""}
          {problem.instance ? ` · ${problem.instance}` : ""}
        </div>
      </div>
      {onDismiss !== undefined ? (
        <button
          type="button"
          className="button button--quiet problem__dismiss"
          onClick={onDismiss}
          aria-label="Dismiss"
        >
          <X size={16} strokeWidth={2} aria-hidden="true" />
        </button>
      ) : null}
    </div>
  );
}

/**
 * The shell's global banner: whatever the mutation cache last reported
 * (`api/queryClient.ts`), shown above every route until dismissed.
 */
export function GlobalProblemBanner() {
  const reported = useSyncExternalStore(problemBanner.subscribe, problemBanner.get, () => null);
  if (reported === null) {
    return null;
  }
  return (
    <ProblemBanner
      problem={reported.problem}
      source={reported.source}
      variant="banner"
      onDismiss={() => {
        problemBanner.dismiss();
      }}
    />
  );
}
