/**
 * A refusal at two lengths: the sentence, and the word.
 *
 * # Why a word is needed at all
 *
 * `ActionButton` puts the full reason in a floating tooltip. Inside a
 * `.ledger-scroll` the stylesheet suppresses that tooltip outright
 * (`.ledger-scroll .button[data-reason]::after { content: none }`), because a
 * scroll container clips an absolutely-positioned descendant — and a hidden
 * one still counts toward the container's overflow, which put phantom
 * scrollbars on tables that fit. A disabled control in a ledger would
 * therefore show a sighted mouse user a dead button and no reason at all.
 *
 * The console's answer, from `ReplicationTable` and the browse file table, is
 * two lengths: a short word visible in the cell with no hover, and the full
 * sentence still on the button's `aria-describedby` sibling and its native
 * `title`. This is where a dialog gets both.
 *
 * # The sentence is never rewritten here
 *
 * `full` is `useCapabilityReason`'s own string, verbatim — one place words a
 * refusal, and it is not this one. Only the word is added, and it is chosen
 * from the *state* of `/me` rather than by matching on the sentence's text,
 * so a reworded sentence cannot silently turn "we could not ask" into "you
 * may not".
 */

import { useMe } from "../../api/hooks";
import type { Capabilities } from "../../api/types";
import { useCapabilityReason } from "../useCapabilityReason";

/** One refusal, at both the lengths a ledger and a page need. */
export interface Refusal {
  /** The whole sentence, for `ActionButton.disabledReason`. */
  full: string;
  /** A few words, for a cell where the tooltip cannot be shown. */
  short: string;
}

/**
 * Why this capability is refused in this namespace, or `undefined` when it is
 * allowed.
 *
 * The namespace is the one the **object being written** lives in, never the
 * page's scope (addenda item 17) — the same rule `useCapabilityReason` keeps,
 * which this delegates to.
 */
export function useRefusal(
  namespace: string | undefined,
  capability: keyof Capabilities | readonly (keyof Capabilities)[],
): Refusal | undefined {
  const full = useCapabilityReason(namespace, capability);
  // Already fetched by the line above and answered from the cache: the key is
  // the same, so this costs no extra request.
  const me = useMe(namespace);
  if (full === undefined) {
    return undefined;
  }
  if (me.isError) {
    // A refused or failed `/me` is not "you may not" — it is "nobody can say",
    // and the word must not claim an RBAC verdict the console never received.
    return { full, short: "cannot tell" };
  }
  if (me.data === undefined) {
    return { full, short: "checking" };
  }
  return { full, short: "not permitted" };
}
