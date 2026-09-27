/**
 * Loading: the silhouette of the ledger rows that are coming, not a spinner.
 *
 * A skeleton keeps the page's shape stable while the answer arrives, so the
 * eye already knows where to look; a spinner in the middle of the content
 * tells the operator nothing and shifts everything when it leaves.
 */
export interface LoadingStateProps {
  /** What is loading, for assistive technology: "snapshots", "the repository". */
  what?: string | undefined;
  /** How many row silhouettes to draw. */
  rows?: number | undefined;
}

const WIDTHS = ["70%", "85%", "55%", "60%", "40%"];

export function LoadingState({ what = "content", rows = 5 }: LoadingStateProps) {
  return (
    <div className="skeleton-ledger" role="status" aria-busy="true" aria-live="polite">
      <span className="visually-hidden">Loading {what}…</span>
      {Array.from({ length: rows }, (_, row) => (
        <div className="skeleton-ledger__row" key={row} aria-hidden="true">
          {WIDTHS.map((width, column) => (
            <span
              className="skeleton"
              key={column}
              style={{ width, animationDelay: `${(row * 80).toString()}ms` }}
            />
          ))}
        </div>
      ))}
    </div>
  );
}
