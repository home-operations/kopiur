import { useState } from "react";

import { usePolicies, useRepositories } from "../api/hooks";
import { useCurrentNamespace } from "../util/namespace";
import { KindChip } from "./KindMark";
import { useNamespaceOptions } from "./namespaceOptions";
import { repositoryKey } from "./pickerChoices";
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
  /** What choosing nothing means; the list then has a Clear button. */
  emptyLabel: string;
  hint?: string | undefined;
  strategy?: "absolute" | "fixed" | undefined;
}

/**
 * A form's namespace, picked from the switcher's list. A namespace the caller
 * cannot list can still be typed into the filter and taken as it is.
 */
export function NamespaceField({
  value,
  onChange,
  ...field
}: FieldProps & { value: string; onChange: (value: string) => void }) {
  const [opened, onOpen] = useOpened();
  const { options } = useNamespaceOptions(opened);
  return (
    <PickerField
      {...field}
      value={value}
      onChange={onChange}
      options={options}
      search="namespaces"
      freeform
      onOpen={onOpen}
    />
  );
}

/**
 * `SnapshotPolicy` names, any number, picked from the policies in the shell's
 * scope. One name may exist in several namespaces; it is listed once, with
 * where.
 */
export function PolicyField({
  value,
  onChange,
  ...field
}: FieldProps & { value: readonly string[]; onChange: (value: string[]) => void }) {
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
      {...field}
      multiple
      value={value}
      onChange={onChange}
      options={options}
      search="policies"
      freeform
      onOpen={onOpen}
    />
  );
}

/**
 * Repositories, any number, picked from the ones in the shell's scope —
 * namespaced and cluster-scoped together, each with its kind chip. A choice
 * is the repository's qualified key, which carries its kind and namespace, so
 * two repositories with one name are never confused; a typed name is sent
 * bare and located in the listing's namespace.
 */
export function RepositoryField({
  value,
  onChange,
  ...field
}: FieldProps & { value: readonly string[]; onChange: (value: string[]) => void }) {
  const [opened, onOpen] = useOpened();
  const repositories = useRepositories(useCurrentNamespace(), { enabled: opened });
  const options: PickerOption[] = (repositories.isSuccess ? repositories.data : []).map((repo) => {
    const cluster = repo.kindPath === "cluster-repository";
    return {
      value: repositoryKey(repo.kindPath, repo.namespace, repo.name),
      label: repo.name,
      icon: <KindChip kind={cluster ? "clusterRepository" : "repository"} size="sm" />,
      meta: cluster ? "cluster" : (repo.namespace ?? undefined),
    };
  });
  return (
    <PickerField
      {...field}
      multiple
      value={value}
      onChange={onChange}
      options={options}
      search="repositories"
      freeform
      // Before the list is fetched, a key still reads as the name.
      labelFor={(key) => key.split("/").at(-1) ?? key}
      onOpen={onOpen}
    />
  );
}
