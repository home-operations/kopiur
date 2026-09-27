/**
 * The reason a control is disabled, asked **for the namespace being acted
 * on** — not for the page's own scope.
 *
 * `/me`'s flags are namespace-scoped (addenda item 17): the backend runs its
 * access reviews for the `?namespace=` it was given, and with none it reviews
 * cluster-scoped, so a user holding a single namespaced RoleBinding sees every
 * control disabled with a reason that is not true. The namespace that decides
 * an action is the one the *object* lives in, which on a cluster-wide list is
 * per row — so this is a hook a row-level control calls with its own
 * namespace, and TanStack's cache collapses the repeats into one request per
 * distinct namespace.
 *
 * `undefined` means allowed. Everything else is a sentence for
 * `ActionButton.disabledReason`, which keeps the control visible, focusable
 * and explained rather than hiding it.
 */

import { useMe } from "../api/hooks";
import type { Capabilities } from "../api/types";
import { capabilityReason } from "./capabilities";

export function useCapabilityReason(
  namespace: string | undefined,
  capability: keyof Capabilities | readonly (keyof Capabilities)[],
): string | undefined {
  const me = useMe(namespace);
  if (me.isError) {
    // A refused or failed `/me` is not "you may not" — it is "nobody can say".
    // Blocking the control is still right (the write would be a guess), but
    // the reason must not claim an RBAC verdict the console never received.
    return `Your permissions could not be read: ${me.error.problem.what} ${me.error.problem.fix}`;
  }
  return capabilityReason(me.data, capability, namespace);
}
