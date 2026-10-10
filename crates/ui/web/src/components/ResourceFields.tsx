import { useState } from "react";

import { usePolicies, useRepositories } from "../api/hooks";
import { useCurrentNamespace } from "../util/namespace";
import { KindChip } from "./KindMark";
import { useNamespaceOptions } from "./namespaceOptions";
import { PickerField } from "./PickerField";
import type { PickerOption } from "./PickerOptions";

/** Fetch only after the field's list is first opened. */
function useOpened(): [boolean, () => void] {
  const [opened, setOpened] = useState(false);
  return [
    opened,
    () => {
      setOpened(true);
    },
  ];
}

interface FieldProps {
  id: string;
  label: string;
  value: string;
  onChange: (value: string) => void;
  /** What leaving it empty means. */
  emptyLabel: string;
  hint?: string | undefined;
  strategy?: "absolute" | "fixed" | undefined;
}

/**
 * A form's namespace, picked from the switcher's list. A namespace the caller
 * cannot list can still be typed into the filter and taken as it is.
 */
export function NamespaceField({
  id,
  label,
  value,
  onChange,
  emptyLabel,
  hint,
  strategy,
}: FieldProps) {
  const [opened, onOpen] = useOpened();
  const { options } = useNamespaceOptions(opened);
  return (
    <PickerField
      id={id}
      label={label}
      value={value}
      onChange={onChange}
      options={options}
      emptyLabel={emptyLabel}
      emptyInitial="*"
      search="namespaces"
      freeform
      onOpen={onOpen}
      hint={hint}
      strategy={strategy}
    />
  );
}

/**
 * A `SnapshotPolicy` by name, picked from the policies in the shell's scope.
 * One name may exist in several namespaces; it is listed once, with where.
 */
export function PolicyField({
  id,
  label,
  value,
  onChange,
  emptyLabel,
  hint,
  strategy,
}: FieldProps) {
  const [opened, onOpen] = useOpened();
  const policies = usePolicies(useCurrentNamespace(), { enabled: opened });
  const where = new Map<string, string[]>();
  for (const policy of policies.isSuccess ? policies.data : []) {
    where.set(policy.name, [...(where.get(policy.name) ?? []), policy.namespace]);
  }
  const options: PickerOption[] = [...where].map(([name, namespaces]) => ({
    value: name,
    label: name,
    icon: <KindChip kind="snapshotPolicy" size="sm" />,
    meta: namespaces.join(", "),
  }));
  return (
    <PickerField
      id={id}
      label={label}
      value={value}
      onChange={onChange}
      options={options}
      emptyLabel={emptyLabel}
      search="policies"
      freeform
      onOpen={onOpen}
      hint={hint}
      strategy={strategy}
    />
  );
}

/** A repository as a snapshot filter names it: its name, kind and namespace. */
export interface RepositoryChoice {
  name: string;
  /** `repository` / `cluster-repository`, or `""` for not said. */
  kind: string;
  /** `""` for not said (and always for a ClusterRepository). */
  namespace: string;
}

const KEY_SEP = "\u0000";

function repositoryKey(kind: string, namespace: string, name: string): string {
  return [kind, namespace, name].join(KEY_SEP);
}

/**
 * A repository, picked from the ones in the shell's scope — namespaced and
 * cluster-scoped together. Picking one sets
 * its kind and namespace too, since a name alone may be ambiguous; a typed
 * name sets the name only.
 */
export function RepositoryField({
  id,
  label,
  value,
  onChange,
  emptyLabel,
  hint,
  strategy,
}: Omit<FieldProps, "value" | "onChange"> & {
  value: RepositoryChoice;
  onChange: (value: RepositoryChoice) => void;
}) {
  const [opened, onOpen] = useOpened();
  const repositories = useRepositories(useCurrentNamespace(), { enabled: opened });
  // Every kind is listed, even when one is chosen: picking sets the kind, so
  // narrowing to it would leave no way back to the other.
  const listed = repositories.isSuccess ? repositories.data : [];
  const options: PickerOption[] = listed.map((repo) => {
    const cluster = repo.kindPath === "cluster-repository";
    return {
      value: repositoryKey(repo.kindPath, cluster ? "" : (repo.namespace ?? ""), repo.name),
      label: repo.name,
      icon: <KindChip kind={cluster ? "clusterRepository" : "repository"} size="sm" />,
      meta: cluster ? "cluster" : (repo.namespace ?? undefined),
    };
  });
  const exact = repositoryKey(value.kind, value.namespace, value.name);
  const current = options.some((option) => option.value === exact) ? exact : value.name;
  return (
    <PickerField
      id={id}
      label={label}
      value={current}
      valueLabel={value.name}
      onChange={(next, option) => {
        if (option === undefined) {
          onChange({ ...value, name: next });
          return;
        }
        const [kind = "", namespace = "", name = ""] = option.value.split(KEY_SEP);
        onChange({ kind, namespace, name });
      }}
      options={options}
      emptyLabel={emptyLabel}
      search="repositories"
      freeform
      onOpen={onOpen}
      hint={hint}
      strategy={strategy}
    />
  );
}
