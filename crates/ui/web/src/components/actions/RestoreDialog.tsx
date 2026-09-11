import { ArchiveRestore } from "lucide-react";
import { type ReactNode, useId, useState } from "react";

import { useCreateRestore } from "../../api/hooks";
import type {
  RepositoryRefBody,
  RestoreBody,
  RestoreSourceBody,
  RestoreTargetBody,
} from "../../api/types";
import { assertNever } from "../../util/assertNever";
import { useCapabilityReason } from "../useCapabilityReason";
import { ActionPanel } from "./ActionPanel";

/**
 * Create a `Restore`: every source the wire type allows, every target it
 * allows, and the one field that decides whether existing data survives.
 *
 * # The destructive lever is `overwrite`, not the target
 *
 * It is tempting to read "restore into an existing PVC" as the dangerous
 * choice and "restore into a new PVC" as the safe one. That is the wrong
 * axis. `RestoreBody.overwrite` maps to `spec.options.overwriteFiles`, which
 * is the `--[no-]overwrite-files` flag the mover passes to kopia — and
 * leaving it unset emits **no flag at all**, so kopia's own default applies,
 * and kopia's default is to overwrite. An unset `overwrite` is therefore a
 * destructive `overwrite`, wearing the clothes of an unanswered question.
 *
 * So this dialog never leaves it unset: it is an unanswered pair of radios,
 * the confirm button is blocked until one is chosen, and the confirmation
 * names the field (addenda item 23). The target still matters and is still
 * said — a claim the operator creates has nothing in it to lose — but it is
 * said as context for the lever, not as the lever.
 *
 * # `sourcePath` is a source selector, and one source has none
 *
 * It picks which of a repository's kopia source paths to READ from, for a
 * multi-PVC policy where each member wrote its own `/pvc/<name>`. It does not
 * restore part of a snapshot. A `snapshotRef` source is already one selected
 * source, so the handler *refuses* the combination outright rather than
 * dropping the field — the input is therefore not offered for that source,
 * with the reason, instead of being sent into a 400.
 *
 * # What is not offered
 *
 * `spec.target.populator` has no wire variant: a populator restore is claimed
 * by a PVC's `dataSourceRef` and is authored in Git beside that PVC, not
 * fired from a button. The missing-snapshot policy, credential projection and
 * the mover's failure controls are deployment policy; the handler leaves them
 * unset so the operator's defaults apply, exactly as a bare `kubectl kopiur
 * restore` does.
 */
export interface RestoreDialogProps {
  /**
   * The namespace the `Restore` is created in — and, because a restore only
   * ever writes into its own namespace, the namespace of the target claim.
   * `createRestores` is reviewed here (addenda item 17).
   */
  namespace: string;
  /** Pre-fill a `snapshotRef` source, for a button on a snapshot's own page. */
  snapshot?: { namespace: string; name: string } | undefined;
  open?: boolean | undefined;
  onOpenChange?: ((open: boolean) => void) | undefined;
}

type SourceKind = "snapshotRef" | "fromPolicy" | "identity";
type TargetKind = "pvcRef" | "pvc";
/** The overwrite question's three states: unanswered, and the two answers. */
type OverwriteChoice = "" | "yes" | "no";

export function RestoreDialog({ namespace, snapshot, open, onOpenChange }: RestoreDialogProps) {
  const fieldId = useId();
  const restore = useCreateRestore();
  const reason = useCapabilityReason(namespace, "createRestores");

  const [sourceKind, setSourceKind] = useState<SourceKind>("snapshotRef");
  const [snapshotName, setSnapshotName] = useState(snapshot?.name ?? "");
  const [snapshotNamespace, setSnapshotNamespace] = useState(snapshot?.namespace ?? "");
  const [policyName, setPolicyName] = useState("");
  const [policyNamespace, setPolicyNamespace] = useState("");
  const [asOf, setAsOf] = useState("");
  const [offset, setOffset] = useState("");
  const [username, setUsername] = useState("");
  const [hostname, setHostname] = useState("");
  const [identityPath, setIdentityPath] = useState("");
  const [snapshotId, setSnapshotId] = useState("");

  const [targetKind, setTargetKind] = useState<TargetKind>("pvcRef");
  const [claimName, setClaimName] = useState("");
  const [newClaimName, setNewClaimName] = useState("");
  const [storageClass, setStorageClass] = useState("");
  const [size, setSize] = useState("");

  const [overwrite, setOverwrite] = useState<OverwriteChoice>("");
  const [restoreName, setRestoreName] = useState("");
  const [sourcePath, setSourcePath] = useState("");
  const [repositoryKind, setRepositoryKind] = useState("");
  const [repositoryName, setRepositoryName] = useState("");
  const [repositoryNamespace, setRepositoryNamespace] = useState("");

  const source = buildSource(sourceKind, {
    snapshotName,
    snapshotNamespace,
    policyName,
    policyNamespace,
    asOf,
    offset,
    username,
    hostname,
    identityPath,
    snapshotId,
  });
  const target = buildTarget(targetKind, { claimName, newClaimName, storageClass, size });
  const repository = buildRepository(repositoryKind, repositoryName, repositoryNamespace);
  // `sourcePath` is meaningless for a snapshotRef and the handler refuses it,
  // so it is never carried into the body from a stale input.
  const selector = sourceKind === "snapshotRef" ? "" : sourcePath.trim();

  const body: RestoreBody | undefined =
    source === undefined || target === undefined || overwrite === ""
      ? undefined
      : {
          namespace,
          source,
          target,
          overwrite: overwrite === "yes",
          ...(restoreName.trim().length > 0 ? { name: restoreName.trim() } : {}),
          ...(selector.length > 0 ? { sourcePath: selector } : {}),
          ...(repository !== undefined ? { repository } : {}),
        };

  return (
    <ActionPanel
      label="Restore"
      icon={ArchiveRestore}
      variant="danger"
      disabledReason={reason}
      confirmLabel="Create the restore"
      blockedReason={blockedReason(source, target, overwrite, repositoryKind, repositoryName)}
      running={restore.isPending}
      onConfirm={() => {
        if (body !== undefined) {
          restore.mutate(body);
        }
      }}
      receipt={restore.data}
      problem={restore.error?.problem}
      open={open}
      onOpenChange={onOpenChange}
    >
      <p>
        This creates a <span className="mono">Restore</span> in{" "}
        <span className="mono">{namespace}</span>. The operator resolves the source, pins it, and
        runs a mover Job — nothing is copied by pressing this button, and the restore&apos;s own
        page is where the outcome appears.
      </p>

      <fieldset className="action__choice">
        <legend>Source</legend>
        <Radio
          id={`${fieldId}-src-snapshot`}
          group={`${fieldId}-src`}
          checked={sourceKind === "snapshotRef"}
          onPick={() => {
            setSourceKind("snapshotRef");
          }}
        >
          A <span className="mono">Snapshot</span> resource, by name.
        </Radio>
        <Radio
          id={`${fieldId}-src-policy`}
          group={`${fieldId}-src`}
          checked={sourceKind === "fromPolicy"}
          onPick={() => {
            setSourceKind("fromPolicy");
          }}
        >
          The latest snapshot a <span className="mono">SnapshotPolicy</span> produced, optionally
          stepped back in time.
        </Radio>
        <Radio
          id={`${fieldId}-src-identity`}
          group={`${fieldId}-src`}
          checked={sourceKind === "identity"}
          onPick={() => {
            setSourceKind("identity");
          }}
        >
          A kopia identity — <span className="mono">user@host:/path</span> — which reaches snapshots
          this operator never took.
        </Radio>
      </fieldset>

      {sourceKind === "snapshotRef" ? (
        <>
          <Field
            id={`${fieldId}-snap`}
            label="Snapshot name"
            value={snapshotName}
            set={setSnapshotName}
          />
          <Field
            id={`${fieldId}-snap-ns`}
            label="Snapshot namespace (optional)"
            value={snapshotNamespace}
            set={setSnapshotNamespace}
            placeholder={namespace}
          />
        </>
      ) : null}

      {sourceKind === "fromPolicy" ? (
        <>
          <Field id={`${fieldId}-pol`} label="Policy name" value={policyName} set={setPolicyName} />
          <Field
            id={`${fieldId}-pol-ns`}
            label="Policy namespace (optional)"
            value={policyNamespace}
            set={setPolicyNamespace}
            placeholder={namespace}
          />
          <Field
            id={`${fieldId}-asof`}
            label="As of (optional)"
            value={asOf}
            set={setAsOf}
            placeholder="2026-09-08T02:00:00Z"
          />
          <Field
            id={`${fieldId}-offset`}
            label="Steps back (optional)"
            value={offset}
            set={setOffset}
            placeholder="0 is the latest"
          />
        </>
      ) : null}

      {sourceKind === "identity" ? (
        <>
          <Field id={`${fieldId}-user`} label="Kopia username" value={username} set={setUsername} />
          <Field id={`${fieldId}-host`} label="Kopia hostname" value={hostname} set={setHostname} />
          <Field
            id={`${fieldId}-ipath`}
            label="Source path (optional)"
            value={identityPath}
            set={setIdentityPath}
            placeholder="/data"
          />
          <Field
            id={`${fieldId}-manifest`}
            label="Kopia manifest ID (optional)"
            value={snapshotId}
            set={setSnapshotId}
            placeholder="the newest match"
          />
        </>
      ) : null}

      <fieldset className="action__choice">
        <legend>Target</legend>
        <Radio
          id={`${fieldId}-tgt-ref`}
          group={`${fieldId}-tgt`}
          checked={targetKind === "pvcRef"}
          onPick={() => {
            setTargetKind("pvcRef");
          }}
        >
          An existing <span className="mono">PersistentVolumeClaim</span> in{" "}
          <span className="mono">{namespace}</span>. Whatever is in it now is what the overwrite
          setting below decides the fate of.
        </Radio>
        <Radio
          id={`${fieldId}-tgt-new`}
          group={`${fieldId}-tgt`}
          checked={targetKind === "pvc"}
          onPick={() => {
            setTargetKind("pvc");
          }}
        >
          A new claim the operator creates for this restore. Nothing exists in it yet, so nothing of
          yours can be lost in it.
        </Radio>
      </fieldset>

      {targetKind === "pvcRef" ? (
        <Field id={`${fieldId}-claim`} label="Claim name" value={claimName} set={setClaimName} />
      ) : (
        <>
          <Field
            id={`${fieldId}-newclaim`}
            label="New claim name"
            value={newClaimName}
            set={setNewClaimName}
          />
          <Field
            id={`${fieldId}-size`}
            label="Size"
            value={size}
            set={setSize}
            placeholder="10Gi"
          />
          <Field
            id={`${fieldId}-sc`}
            label="Storage class (optional)"
            value={storageClass}
            set={setStorageClass}
            placeholder="the cluster default"
          />
        </>
      )}

      <fieldset className="action__choice" data-danger="true">
        <legend>Overwrite existing files</legend>
        <p className="action__note">
          This is <span className="mono">overwrite</span>, which becomes{" "}
          <span className="mono">spec.options.overwriteFiles</span> and then kopia&apos;s{" "}
          <span className="mono">--[no-]overwrite-files</span>. Left unset it sends no flag at all
          and kopia&apos;s own default — overwrite — applies, so there is no neutral answer and this
          dialog always sends one.
        </p>
        <Radio
          id={`${fieldId}-ow-no`}
          group={`${fieldId}-ow`}
          checked={overwrite === "no"}
          onPick={() => {
            setOverwrite("no");
          }}
        >
          Leave existing files alone. A file already at the target keeps the contents it has.
        </Radio>
        <Radio
          id={`${fieldId}-ow-yes`}
          group={`${fieldId}-ow`}
          checked={overwrite === "yes"}
          onPick={() => {
            setOverwrite("yes");
          }}
        >
          Overwrite them. Every file the snapshot carries replaces the one at the target, and{" "}
          <strong>what is there now is gone</strong>.
        </Radio>
      </fieldset>

      {sourceKind === "snapshotRef" ? (
        <p className="action__note">
          A source path cannot be chosen for this source: the <span className="mono">Snapshot</span>{" "}
          you named is already one kopia source, and the server refuses the combination rather than
          ignoring it. Use a policy or identity source to pick a different path.
        </p>
      ) : (
        <Field
          id={`${fieldId}-spath`}
          label="Source path (optional)"
          value={sourcePath}
          set={setSourcePath}
          placeholder="/pvc/data"
        />
      )}

      <div className="controls__field">
        <label htmlFor={`${fieldId}-repo-kind`}>Repository (optional)</label>
        <select
          id={`${fieldId}-repo-kind`}
          className="controls__input"
          value={repositoryKind}
          onChange={(event) => {
            setRepositoryKind(event.target.value);
          }}
        >
          <option value="">Infer it from the source</option>
          <option value="Repository">Repository</option>
          <option value="ClusterRepository">ClusterRepository</option>
        </select>
      </div>
      {repositoryKind.length > 0 ? (
        <>
          <Field
            id={`${fieldId}-repo-name`}
            label="Repository name"
            value={repositoryName}
            set={setRepositoryName}
          />
          {repositoryKind === "Repository" ? (
            <Field
              id={`${fieldId}-repo-ns`}
              label="Repository namespace (optional)"
              value={repositoryNamespace}
              set={setRepositoryNamespace}
              placeholder={namespace}
            />
          ) : null}
        </>
      ) : null}

      <Field
        id={`${fieldId}-name`}
        label="Restore name (optional)"
        value={restoreName}
        set={setRestoreName}
        placeholder="the server generates one"
      />
    </ActionPanel>
  );
}

/** Every free-text field the source and target variants need. */
interface SourceInputs {
  snapshotName: string;
  snapshotNamespace: string;
  policyName: string;
  policyNamespace: string;
  asOf: string;
  offset: string;
  username: string;
  hostname: string;
  identityPath: string;
  snapshotId: string;
}

/**
 * The chosen source as the wire union, or `undefined` while a required field
 * of that variant is still empty.
 *
 * Exhaustive over `SourceKind`: a fourth source variant cannot be added to
 * the wire type and quietly go unbuilt here.
 */
function buildSource(kind: SourceKind, input: SourceInputs): RestoreSourceBody | undefined {
  switch (kind) {
    case "snapshotRef": {
      const name = input.snapshotName.trim();
      const ns = input.snapshotNamespace.trim();
      return name.length === 0
        ? undefined
        : { snapshotRef: { name, ...(ns.length > 0 ? { namespace: ns } : {}) } };
    }
    case "fromPolicy": {
      const name = input.policyName.trim();
      if (name.length === 0) {
        return undefined;
      }
      const ns = input.policyNamespace.trim();
      const asOf = input.asOf.trim();
      const steps = Number.parseInt(input.offset.trim(), 10);
      return {
        fromPolicy: {
          name,
          ...(ns.length > 0 ? { namespace: ns } : {}),
          ...(asOf.length > 0 ? { asOf } : {}),
          ...(Number.isFinite(steps) && steps >= 0 ? { offset: steps } : {}),
        },
      };
    }
    case "identity": {
      const username = input.username.trim();
      const hostname = input.hostname.trim();
      if (username.length === 0 || hostname.length === 0) {
        return undefined;
      }
      const path = input.identityPath.trim();
      const id = input.snapshotId.trim();
      return {
        identity: {
          username,
          hostname,
          ...(path.length > 0 ? { sourcePath: path } : {}),
          ...(id.length > 0 ? { snapshotId: id } : {}),
        },
      };
    }
    default:
      return assertNever(kind, "SourceKind");
  }
}

interface TargetInputs {
  claimName: string;
  newClaimName: string;
  storageClass: string;
  size: string;
}

/** The chosen target as the wire union, or `undefined` while incomplete. */
function buildTarget(kind: TargetKind, input: TargetInputs): RestoreTargetBody | undefined {
  switch (kind) {
    case "pvcRef": {
      const name = input.claimName.trim();
      return name.length === 0 ? undefined : { pvcRef: { name } };
    }
    case "pvc": {
      const name = input.newClaimName.trim();
      const size = input.size.trim();
      if (name.length === 0 || size.length === 0) {
        return undefined;
      }
      const storageClassName = input.storageClass.trim();
      return {
        pvc: {
          name,
          size,
          ...(storageClassName.length > 0 ? { storageClassName } : {}),
        },
      };
    }
    default:
      return assertNever(kind, "TargetKind");
  }
}

/**
 * An explicit repository reference, or `undefined` to let the server infer
 * one from the source. A `ClusterRepository` never carries a namespace — the
 * CRD forbids one and the handler drops it.
 */
function buildRepository(
  kind: string,
  name: string,
  namespace: string,
): RepositoryRefBody | undefined {
  const trimmed = name.trim();
  if (kind.length === 0 || trimmed.length === 0) {
    return undefined;
  }
  const ns = namespace.trim();
  return {
    kind,
    name: trimmed,
    ...(kind === "Repository" && ns.length > 0 ? { namespace: ns } : {}),
  };
}

/** Which unanswered question is holding the confirm button, in one sentence. */
function blockedReason(
  source: RestoreSourceBody | undefined,
  target: RestoreTargetBody | undefined,
  overwrite: OverwriteChoice,
  repositoryKind: string,
  repositoryName: string,
): string | undefined {
  if (source === undefined) {
    return "The source is not complete — fill in every field it needs before this restore can be created.";
  }
  if (target === undefined) {
    return "The target is not complete — a restore has to say which claim it writes into, and a new claim needs a size.";
  }
  if (overwrite === "") {
    return "Say what happens to files already at the target. There is no neutral answer: leaving overwrite unset lets kopia's own default overwrite them.";
  }
  if (repositoryKind.length > 0 && repositoryName.trim().length === 0) {
    return "A repository kind was chosen but not named. Name it, or go back to inferring the repository from the source.";
  }
  return undefined;
}

/** One labelled text input, at the console's control size. */
function Field({
  id,
  label,
  value,
  set,
  placeholder,
}: {
  id: string;
  label: string;
  value: string;
  set: (value: string) => void;
  placeholder?: string | undefined;
}) {
  return (
    <div className="controls__field">
      <label htmlFor={id}>{label}</label>
      <input
        id={id}
        className="controls__input"
        value={value}
        placeholder={placeholder}
        onChange={(event) => {
          set(event.target.value);
        }}
      />
    </div>
  );
}

/** One radio in an `action__choice` group, with its consequence beside it. */
function Radio({
  id,
  group,
  checked,
  onPick,
  children,
}: {
  id: string;
  group: string;
  checked: boolean;
  onPick: () => void;
  children: ReactNode;
}) {
  return (
    <label htmlFor={id}>
      <input type="radio" id={id} name={group} checked={checked} onChange={onPick} />
      <span>{children}</span>
    </label>
  );
}
