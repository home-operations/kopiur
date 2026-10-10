import type { ObjectKind } from "../../api/types";
import type { Lamp } from "../health";
import type { InspectTarget } from "../inspect";
import { KIND_META } from "../kind";
import { ObjectRef } from "../ObjectRef";

/** One thing in a step: another resource (opened in the drawer), or text the console cannot name. */
export type ChainItem = { ref: InspectTarget; health?: Lamp | undefined } | { text: string };

/** A step of the chain: what the resources in it are to this one ("Fired by"). */
export interface ChainStep {
  label: string;
  items: readonly ChainItem[];
}

export interface ChainProps {
  /** This resource's kind, for the "this policy" step. */
  kind: ObjectKind;
  /** This resource's namespace: references in it are not prefixed with it. */
  namespace?: string | undefined;
  /** What feeds it, outermost first. */
  before: readonly ChainStep[];
  /** What it feeds, nearest first. */
  after: readonly ChainStep[];
}

/** "this policy", "this repository" — the CRD kind, lowercased, without the Snapshot prefix. */
function thisWord(kind: ObjectKind): string {
  return `this ${KIND_META[kind].label.replace(/^Snapshot(?=Policy|Schedule)/, "").toLowerCase()}`;
}

/**
 * Where a resource sits, as one line read left to right: what feeds it, then
 * it, then what it feeds. A step with nothing in it is left out rather than
 * drawn empty. A reference the console cannot read stays the server's own
 * text — never a link guessed into existence.
 */
export function Chain({ kind, namespace, before, after }: ChainProps) {
  const here = thisWord(kind);
  const steps = [
    ...before.filter((s) => s.items.length > 0),
    { label: here.charAt(0).toUpperCase() + here.slice(1), items: [] as ChainItem[], here: true },
    ...after.filter((s) => s.items.length > 0),
  ];
  return (
    <ol className="chain" aria-label="Where this sits">
      {steps.map((step, index) => (
        <li
          key={step.label}
          className={"here" in step ? "chain__step chain__step--here" : "chain__step"}
          aria-label={step.label}
        >
          {index > 0 ? (
            <span className="chain__arrow" aria-hidden="true">
              →
            </span>
          ) : null}
          {"here" in step ? (
            <span className="chain__here">{here}</span>
          ) : (
            <span className="chain__group">
              <span className="chain__label">{step.label}</span>
              <ul className="chain__items">
                {step.items.map((item) =>
                  "ref" in item ? (
                    <li key={`${item.ref.kind}/${item.ref.namespace ?? ""}/${item.ref.name}`}>
                      <ObjectRef
                        kind={item.ref.kind}
                        name={item.ref.name}
                        namespace={item.ref.namespace}
                        contextNamespace={namespace}
                        health={item.health}
                      />
                    </li>
                  ) : (
                    <li key={`text:${item.text}`} className="mono chain__text">
                      {item.text}
                    </li>
                  ),
                )}
              </ul>
            </span>
          )}
        </li>
      ))}
    </ol>
  );
}
