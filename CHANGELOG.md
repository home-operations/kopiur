# Changelog

## [0.10.0](https://github.com/home-operations/kopiur/compare/0.9.5...0.10.0) (2026-08-09)


### ⚠ BREAKING CHANGES

* **rust:** Update crate base64 (0.22.1 → 0.23.1) ([#366](https://github.com/home-operations/kopiur/issues/366))

### Features

* **rust:** Update crate base64 (0.22.1 → 0.23.1) ([#366](https://github.com/home-operations/kopiur/issues/366)) ([2c6e3e7](https://github.com/home-operations/kopiur/commit/2c6e3e7cb53706fd20d1096342292f583930e671))
* SnapshotReplication (logical snapshot-level replication) + SnapshotPolicy multi-repository fan-out ([#370](https://github.com/home-operations/kopiur/issues/370)) ([4e06e42](https://github.com/home-operations/kopiur/commit/4e06e42f2f9eb8711c17c443d0e51deead45c55d))


### Continuous Integration

* **github-action:** Update action Swatinem/rust-cache (v2.9.1 → v2.9.2) ([#372](https://github.com/home-operations/kopiur/issues/372)) ([e4ce173](https://github.com/home-operations/kopiur/commit/e4ce173b44b37fd0f5bfb5fbc98892d67ccff63f))


### Miscellaneous Chores

* **mise:** Update mise tools ([#367](https://github.com/home-operations/kopiur/issues/367)) ([d6f82f4](https://github.com/home-operations/kopiur/commit/d6f82f488883a0fecb7b41888d216ae8cfe4b3a4))
* **mise:** Update tool cosign (3.1.2 → 3.1.3) ([#371](https://github.com/home-operations/kopiur/issues/371)) ([558b9f4](https://github.com/home-operations/kopiur/commit/558b9f4d487e93e75d7d42ffa59d3e68eea8ed48))

## [0.9.5](https://github.com/home-operations/kopiur/compare/0.9.4...0.9.5) (2026-08-08)


### Bug Fixes

* deliver s3 tls.caBundleRef to every kopia invocation ([#365](https://github.com/home-operations/kopiur/issues/365)) ([cda0237](https://github.com/home-operations/kopiur/commit/cda0237d3ed9d6b980c79125c521ece5a618b79f))
* doctor misses structurally-blocked work ([#359](https://github.com/home-operations/kopiur/issues/359)) + phase/gate exhaustiveness ratchet ([#363](https://github.com/home-operations/kopiur/issues/363)) ([b844974](https://github.com/home-operations/kopiur/commit/b844974fd2f4d899a54315bef990d96146607d91))
* **rust:** update crate thiserror (2.0.19 → 2.0.20) ([#360](https://github.com/home-operations/kopiur/issues/360)) ([4aa646d](https://github.com/home-operations/kopiur/commit/4aa646d9578c1406593769a6ae2db33894e15a22))


### Continuous Integration

* **github-action:** Update action docker/github-builder (v1.15.0 → v1.16.0) ([#362](https://github.com/home-operations/kopiur/issues/362)) ([db32818](https://github.com/home-operations/kopiur/commit/db32818586c75de393c7285fd0204be5ddfa8c5e))

## [0.9.4](https://github.com/home-operations/kopiur/compare/0.9.3...0.9.4) (2026-08-08)


### Features

* **docs:** Update README.md ([#358](https://github.com/home-operations/kopiur/issues/358)) ([05d2506](https://github.com/home-operations/kopiur/commit/05d2506fb7e5a81d4db22abdb68cf78037150e70))


### Bug Fixes

* implement pvcSelector/groupBy ([#346](https://github.com/home-operations/kopiur/issues/346)) and model deduped backups as Unchanged ([#351](https://github.com/home-operations/kopiur/issues/351)) ([#354](https://github.com/home-operations/kopiur/issues/354)) ([a2b9ae6](https://github.com/home-operations/kopiur/commit/a2b9ae6ee5e6de729108a2f7b88894a027b61d95))
* **rust:** update crate clap (4.6.5 → 4.6.6) ([#357](https://github.com/home-operations/kopiur/issues/357)) ([e9ca396](https://github.com/home-operations/kopiur/commit/e9ca3969fdc746b6b1f96f72e7668fab2c2117be))


### Miscellaneous Chores

* **mise:** Update tool node (24.18.1 → v24.19.0) ([#353](https://github.com/home-operations/kopiur/issues/353)) ([2f28c6d](https://github.com/home-operations/kopiur/commit/2f28c6d8daa4f90eb4fc213c0ababdf3b65730ce))
* **mise:** Update tool oxfmt (0.61.0 → 0.62.0) ([#356](https://github.com/home-operations/kopiur/issues/356)) ([e95df6b](https://github.com/home-operations/kopiur/commit/e95df6bade7327bc8ad238e3466e030a08de8082))

## [0.9.3](https://github.com/home-operations/kopiur/compare/0.9.2...0.9.3) (2026-08-05)


### Features

* repository-level circuit breaker — stop fanning out doomed Jobs when the backend is unreachable ([#350](https://github.com/home-operations/kopiur/issues/350)) ([5d787b9](https://github.com/home-operations/kopiur/commit/5d787b98eb606ed8410f57db26cfc1004cd35ce9))


### Bug Fixes

* **rust:** update crate base64 (0.23.0 → 0.23.1) ([#349](https://github.com/home-operations/kopiur/issues/349)) ([11e7aa8](https://github.com/home-operations/kopiur/commit/11e7aa83b324418102537e0f7355aafff4063abb))


### Continuous Integration

* **github-action:** Update action jdx/mise-action (v4.2.3 → v4.2.4) ([#347](https://github.com/home-operations/kopiur/issues/347)) ([1c9cf3b](https://github.com/home-operations/kopiur/commit/1c9cf3b5b31690792bd0e0ff645c2607c7dfaec2))


### Miscellaneous Chores

* **renovate:** keep 0.x minors out of automerge ([#352](https://github.com/home-operations/kopiur/issues/352)) ([95d672a](https://github.com/home-operations/kopiur/commit/95d672af7d97a0b1b539610c208a316316b77e0a))

## [0.9.2](https://github.com/home-operations/kopiur/compare/0.9.1...0.9.2) (2026-08-02)


### Features

* **repository:** object-lock blob retention ([#332](https://github.com/home-operations/kopiur/issues/332)); fix blank volumeSnapshotClassName ([#344](https://github.com/home-operations/kopiur/issues/344)) ([dab8fdd](https://github.com/home-operations/kopiur/commit/dab8fdd84ca35514640a6c76eb473689d9a0c6b8))


### Bug Fixes

* **ci:** fail the merge gate on cancelled jobs, and cut e2e artifact time ([#330](https://github.com/home-operations/kopiur/issues/330)) ([1847a8a](https://github.com/home-operations/kopiur/commit/1847a8a61e050e44af6a574b10555a2b2df96cda))
* **rust:** update crate time (0.3.54 → 0.3.55) ([#337](https://github.com/home-operations/kopiur/issues/337)) ([ee1b951](https://github.com/home-operations/kopiur/commit/ee1b951c12799f9b09f32ad0e26b8a707ca3fb48))


### Continuous Integration

* **github-action:** Update action docker/github-builder (v1.14.0 → v1.15.0) ([#327](https://github.com/home-operations/kopiur/issues/327)) ([f5514fc](https://github.com/home-operations/kopiur/commit/f5514fcb93b70a71e1a495d69fbcc4585e5cb7f6))
* **github-action:** Update action docker/login-action (v4.5.1 → v4.5.2) ([#331](https://github.com/home-operations/kopiur/issues/331)) ([d90efdf](https://github.com/home-operations/kopiur/commit/d90efdfcefee8afe55ebcb7292952db4cdb995f0))
* **github-action:** Update action docker/login-action (v4.5.2 → v4.6.0) ([#339](https://github.com/home-operations/kopiur/issues/339)) ([b783bcc](https://github.com/home-operations/kopiur/commit/b783bcc175d05ce225156aa36fedbf0c20a90db3))
* **github-action:** Update action home-operations/.github/actions/workflow-lint (v1.0.2 → v1.0.3) ([#343](https://github.com/home-operations/kopiur/issues/343)) ([4af16d5](https://github.com/home-operations/kopiur/commit/4af16d5b3863fb97537840a74db14e504f960ec1))
* update shared actions and use self-repository syntax ([#341](https://github.com/home-operations/kopiur/issues/341)) ([559e23f](https://github.com/home-operations/kopiur/commit/559e23f02e26895ed4f0d58346c9d19123824f42))


### Miscellaneous Chores

* **deps:** lock file maintenance (pep621) ([#334](https://github.com/home-operations/kopiur/issues/334)) ([e19ae0e](https://github.com/home-operations/kopiur/commit/e19ae0e65728496caa378e846fc7168b0c6a3198))
* **mise:** Lock file maintenance tool (mise) ([#335](https://github.com/home-operations/kopiur/issues/335)) ([666afb5](https://github.com/home-operations/kopiur/commit/666afb5af1abe1f42b9f006c2c953bf67a5f5060))
* **mise:** prune lockfile to used platforms ([#342](https://github.com/home-operations/kopiur/issues/342)) ([741d6a4](https://github.com/home-operations/kopiur/commit/741d6a4bf6dd7aba99f4b46178c4faf3643d4698))
* **mise:** Update tool aqua:astral-sh/uv (0.12.0 → 0.12.1) ([#333](https://github.com/home-operations/kopiur/issues/333)) ([3ed2766](https://github.com/home-operations/kopiur/commit/3ed276634bac7609b7a2e04146912cbfff10942e))
* **mise:** Update tool promtool (3.13.1 → 3.13.2) ([#329](https://github.com/home-operations/kopiur/issues/329)) ([ac1752e](https://github.com/home-operations/kopiur/commit/ac1752e296328fd41b178f76905930fd0b6b9258))
* **mise:** Update tool zizmor (1.28.0 → 1.29.0) ([#340](https://github.com/home-operations/kopiur/issues/340)) ([410dc26](https://github.com/home-operations/kopiur/commit/410dc2671ddd46000cb17057460ca15f7c55ddce))
* **release-please:** standardize the release pull request title pattern ([#338](https://github.com/home-operations/kopiur/issues/338)) ([21013a3](https://github.com/home-operations/kopiur/commit/21013a318c7ef16f0685798c12d6357e671a8ff2))
* **rust:** lock file maintenance crate (cargo) ([#336](https://github.com/home-operations/kopiur/issues/336)) ([8606177](https://github.com/home-operations/kopiur/commit/8606177aa55ed4e46a87e238410f306120e60b92))

## [0.9.1](https://github.com/home-operations/kopiur/compare/0.9.0...0.9.1) (2026-07-29)


### Features

* **rust:** update crate http (1.4.2 → 1.5.0) ([#323](https://github.com/home-operations/kopiur/issues/323)) ([cf0b149](https://github.com/home-operations/kopiur/commit/cf0b14938c6d0f34e7c0dfd0e7520a6c5f1d0fbf))


### Bug Fixes

* **controller:** survive a slow API server instead of abdicating on the first one ([#319](https://github.com/home-operations/kopiur/issues/319)) ([#324](https://github.com/home-operations/kopiur/issues/324)) ([df7e90c](https://github.com/home-operations/kopiur/commit/df7e90cbef115f95e6d3ea5cef9d2de1d118d8f6))
* **rust:** update crate rustls (0.23.42 → 0.23.43) ([#326](https://github.com/home-operations/kopiur/issues/326)) ([2f5123e](https://github.com/home-operations/kopiur/commit/2f5123ee7576e5d07a23373d0d47f2161342b95b))


### Tests

* **e2e:** retry flaky tests with cargo-nextest ([#322](https://github.com/home-operations/kopiur/issues/322)) ([558ac85](https://github.com/home-operations/kopiur/commit/558ac85d837148381c19bdbae1ff1cb1c4470156))


### Miscellaneous Chores

* **mise:** Update tool aqua:astral-sh/uv (0.11.33 → 0.12.0) ([#320](https://github.com/home-operations/kopiur/issues/320)) ([66ec350](https://github.com/home-operations/kopiur/commit/66ec350e349a54c4d8c7abaf9cb5a3844e18566c))
* **mise:** Update tool node (24.18.0 → v24.18.1) ([#325](https://github.com/home-operations/kopiur/issues/325)) ([1c833a4](https://github.com/home-operations/kopiur/commit/1c833a4e5eed2816445b102b5e1003a0d470357e))

## [0.9.0](https://github.com/home-operations/kopiur/compare/0.8.1...0.9.0) (2026-07-28)


### ⚠ BREAKING CHANGES

* **rust:** Update crate serial_test (3.5.0 → 4.0.1) ([#307](https://github.com/home-operations/kopiur/issues/307))
* **secctx:** identity-aware mover merge, recorded snapshot identity, restore inherit-from-snapshot ([#304](https://github.com/home-operations/kopiur/issues/304))

### Features

* **rust:** Update crate serial_test (3.5.0 → 4.0.1) ([#307](https://github.com/home-operations/kopiur/issues/307)) ([4392e4a](https://github.com/home-operations/kopiur/commit/4392e4a8e286df589f4f5ba12149fa8d4ce1e2de))
* **secctx:** identity-aware mover merge, recorded snapshot identity, restore inherit-from-snapshot ([#304](https://github.com/home-operations/kopiur/issues/304)) ([0632a29](https://github.com/home-operations/kopiur/commit/0632a2914e90e703a67f912167551f81ae873b07))


### Bug Fixes

* **rust:** update crate cel (0.14.0 → 0.14.1) ([#317](https://github.com/home-operations/kopiur/issues/317)) ([00cf539](https://github.com/home-operations/kopiur/commit/00cf539af433b11b1d0cf395bd5294feb6a9f5e8))
* **rust:** update crate schemars (1.2.1 → 1.2.2) ([#313](https://github.com/home-operations/kopiur/issues/313)) ([b6e6268](https://github.com/home-operations/kopiur/commit/b6e62688ff7ebac47bbce0cd04b123173115217d))


### Build System

* **mise:** add actionlint and refresh the lockfile ([#309](https://github.com/home-operations/kopiur/issues/309)) ([e74c9b3](https://github.com/home-operations/kopiur/commit/e74c9b37891f51ad8c1b47bf9ae993d0f657bf0d))


### Continuous Integration

* adopt the shared workflow-lint and docs-build actions ([#310](https://github.com/home-operations/kopiur/issues/310)) ([7d4b329](https://github.com/home-operations/kopiur/commit/7d4b3297341c16ec2a7d0c6b57e3db1c40363842))
* gate pull requests on Build Success and share the docs build ([#306](https://github.com/home-operations/kopiur/issues/306)) ([1b693aa](https://github.com/home-operations/kopiur/commit/1b693aa6670e771b3676a509c106f74215e5f3f4))
* **github-action:** Update action docker/login-action (v4.5.0 → v4.5.1) ([#312](https://github.com/home-operations/kopiur/issues/312)) ([29d00b1](https://github.com/home-operations/kopiur/commit/29d00b1a3a02b84efbcc2b2f5723544e24e84ce5))
* **github-action:** Update action jdx/mise-action (v4.2.1 → v4.2.2) ([#311](https://github.com/home-operations/kopiur/issues/311)) ([65bbca4](https://github.com/home-operations/kopiur/commit/65bbca419d6d01471efcd231a4b55ecb7713eaae))
* **github-action:** Update action jdx/mise-action (v4.2.2 → v4.2.3) ([#315](https://github.com/home-operations/kopiur/issues/315)) ([5f517d4](https://github.com/home-operations/kopiur/commit/5f517d4184df71486684695b82c8b205058a638a))
* skip release-please PRs in checks and drop nightly e2e ([#305](https://github.com/home-operations/kopiur/issues/305)) ([ee6d4f5](https://github.com/home-operations/kopiur/commit/ee6d4f57b935e1c9d160cb3161878047fd490146))


### Miscellaneous Chores

* **krew:** kopiur 0.8.1 manifest ([84df757](https://github.com/home-operations/kopiur/commit/84df7575c9c0c139f0199b530236052b082b422e))
* **mise:** Update tool aqua:astral-sh/uv (0.11.32 → 0.11.33) ([#318](https://github.com/home-operations/kopiur/issues/318)) ([f63bc3a](https://github.com/home-operations/kopiur/commit/f63bc3a33ad1ee96876889e5078648922cc0bd3f))
* **mise:** Update tool oxfmt (0.60.0 → 0.61.0) ([#314](https://github.com/home-operations/kopiur/issues/314)) ([bdbf6c6](https://github.com/home-operations/kopiur/commit/bdbf6c6bb50cfca0dc18f3e074422e108ebc6c9f))
* **renovate:** drop the stale automerge overrides ([#308](https://github.com/home-operations/kopiur/issues/308)) ([c13a3d5](https://github.com/home-operations/kopiur/commit/c13a3d5d4d0aa507de2c2b24769b83029d1b9ca2))
* standardize release-please changelog sections ([#316](https://github.com/home-operations/kopiur/issues/316)) ([273d0f8](https://github.com/home-operations/kopiur/commit/273d0f833b52b04652bc7fd4bc10844d4a5c4881))

## [0.8.1](https://github.com/home-operations/kopiur/compare/0.8.0...0.8.1) (2026-07-24)


### Features

* **deps:** update rust crate base64 (0.22.1 → 0.23.0) ([#293](https://github.com/home-operations/kopiur/issues/293)) ([5ab14d5](https://github.com/home-operations/kopiur/commit/5ab14d5e7d19af218c0273c70ce3d42ac05064da))
* **deps:** update rust crate kube (4.0.0 → 4.2.0) ([#288](https://github.com/home-operations/kopiur/issues/288)) ([b067458](https://github.com/home-operations/kopiur/commit/b0674583347dd4b017b15256c8a6a5a778f7db5a))
* httpRequest hook headers ([#290](https://github.com/home-operations/kopiur/issues/290)) + recovery-aware alerts ([#280](https://github.com/home-operations/kopiur/issues/280)) ([#296](https://github.com/home-operations/kopiur/issues/296)) ([f388a14](https://github.com/home-operations/kopiur/commit/f388a14f6a0310fdd2d18ebd4e5f3d85ce84948b))
* **renovate:** automerge if it passes CI ([66f243f](https://github.com/home-operations/kopiur/commit/66f243f3e763504b567d78489488c32d4253e5bc))


### Bug Fixes

* **controller:** survive an API-server outage without exhausting file descriptors ([#287](https://github.com/home-operations/kopiur/issues/287)) ([ce184b6](https://github.com/home-operations/kopiur/commit/ce184b6835fcc02926dfb373993ee122311811ca))
* **deps:** update rust crate clap (4.6.2 → 4.6.3) ([#277](https://github.com/home-operations/kopiur/issues/277)) ([e3d4113](https://github.com/home-operations/kopiur/commit/e3d4113034ce1a327c688ecc316dd7b6fd15fbe2))
* **deps:** update rust crate libc (0.2.186 → 0.2.189) ([#283](https://github.com/home-operations/kopiur/issues/283)) ([2b2e636](https://github.com/home-operations/kopiur/commit/2b2e6363aa99496ba3233d400a97b4a93473a13b))
* **deps:** update rust crate serde_json (1.0.150 → 1.0.151) ([#275](https://github.com/home-operations/kopiur/issues/275)) ([1410f42](https://github.com/home-operations/kopiur/commit/1410f42f97e94d96ad11ded78bc71591b97043e3))
* **deps:** update rust crate time (0.3.53 → 0.3.54) ([#276](https://github.com/home-operations/kopiur/issues/276)) ([ff250ff](https://github.com/home-operations/kopiur/commit/ff250ffdacf7f276d7ecd67fc0d958c1ddddc9d4))
* **deps:** update rust crate tokio (1.53.0 → 1.53.1) ([#281](https://github.com/home-operations/kopiur/issues/281)) ([82082e9](https://github.com/home-operations/kopiur/commit/82082e9323a81c241df9cb003e7db5dbc22bb71e))
* **helm:** stamp Chart.yaml version on release + document image sub-fields ([#289](https://github.com/home-operations/kopiur/issues/289)) ([041549a](https://github.com/home-operations/kopiur/commit/041549a338642855a132cd720fa86bfbb78f992e))
* **probe:** stamp the health probe at launch so it stops recycling its Job ([#273](https://github.com/home-operations/kopiur/issues/273)) ([#278](https://github.com/home-operations/kopiur/issues/278)) ([7fa3657](https://github.com/home-operations/kopiur/commit/7fa36571698908691e24c6abe65a4b1bb705476c))
* **release:** use the generic updater for Chart.yaml + stamp README badges ([#292](https://github.com/home-operations/kopiur/issues/292)) ([f4a3141](https://github.com/home-operations/kopiur/commit/f4a31419b40bd1eba3b56496ef50671e2ca24a43))
* retention-aware adoption stops the adopt/prune/rediscover livelock ([#299](https://github.com/home-operations/kopiur/issues/299)) ([4093f12](https://github.com/home-operations/kopiur/commit/4093f12185b2156c7a50bfeff114e0a6588ecb7d))


### Styles

* indent markdown at 2 to match embedded yaml ([#279](https://github.com/home-operations/kopiur/issues/279)) ([06885e0](https://github.com/home-operations/kopiur/commit/06885e069a0bec4b88ea7de09d3945a7e6652dc0))


### Miscellaneous Chores

* **github-release:** Update release helm-unittest/helm-unittest (v1.1.1 → v1.1.2) ([#301](https://github.com/home-operations/kopiur/issues/301)) ([e62272d](https://github.com/home-operations/kopiur/commit/e62272d7b0567a4e987fd27d5965648abb0d50ab))
* **krew:** kopiur 0.8.0 manifest ([2be416f](https://github.com/home-operations/kopiur/commit/2be416ffd31c367d502ede66fb9a9eaea1465df6))
* **mise:** Update tool aqua:astral-sh/uv (0.11.29 → 0.11.30) ([#282](https://github.com/home-operations/kopiur/issues/282)) ([e12f96c](https://github.com/home-operations/kopiur/commit/e12f96c1e7a798e173b28563facd2adc60f0ba70))
* **mise:** Update tool aqua:astral-sh/uv (0.11.30 → 0.11.31) ([#286](https://github.com/home-operations/kopiur/issues/286)) ([a3f31e6](https://github.com/home-operations/kopiur/commit/a3f31e68a03d98ce90d9eccd9811636ba920e8a0))
* **mise:** Update tool aqua:astral-sh/uv (0.11.31 → 0.11.32) ([#298](https://github.com/home-operations/kopiur/issues/298)) ([9d8c171](https://github.com/home-operations/kopiur/commit/9d8c171a199d52cf256c72c2d85e046111354d32))
* **mise:** Update tool kubectl (1.36.2 → 1.36.3) ([#291](https://github.com/home-operations/kopiur/issues/291)) ([ef290f8](https://github.com/home-operations/kopiur/commit/ef290f81aac128ba54f8a3f10ff4c8b3e0f00dc3))
* **mise:** Update tool oxfmt (0.59.0 → 0.60.0) ([#284](https://github.com/home-operations/kopiur/issues/284)) ([ad8a18a](https://github.com/home-operations/kopiur/commit/ad8a18a15548f0e7dbe5b0dacd1fc69dfad59744))
* **mise:** Update tool rust (1.96.1 → 1.97.1) ([#222](https://github.com/home-operations/kopiur/issues/222)) ([fbeb2fa](https://github.com/home-operations/kopiur/commit/fbeb2faa68d4e7a8790c5a00582a3f1fb1496f78))
* **mise:** Update tool zizmor (1.27.0 → 1.28.0) ([#285](https://github.com/home-operations/kopiur/issues/285)) ([9ef35ce](https://github.com/home-operations/kopiur/commit/9ef35ce8c9605e4be0c26b83a8cea6a71c34cd67))

## [0.8.0](https://github.com/home-operations/kopiur/compare/0.7.5...0.8.0) (2026-07-19)

### ⚠ BREAKING CHANGES

- SnapshotPolicy deletion cascade + auto-adoption of discovered snapshots ([#272](https://github.com/home-operations/kopiur/issues/272))

### Features

- **deps:** update rust crate tokio (1.52.4 → 1.53.0) ([#264](https://github.com/home-operations/kopiur/issues/264)) ([6cab0c6](https://github.com/home-operations/kopiur/commit/6cab0c6d5ce56a1bae48a2b593ca7e57983dcec8))
- mass-deletion protection (cascade guard, per-repo breaker, batched deletes) ([#265](https://github.com/home-operations/kopiur/issues/265)) ([038ea6d](https://github.com/home-operations/kopiur/commit/038ea6d5685b2e8959b29314456332a40e7f5b64))
- SnapshotPolicy deletion cascade + auto-adoption of discovered snapshots ([#272](https://github.com/home-operations/kopiur/issues/272)) ([9da8c26](https://github.com/home-operations/kopiur/commit/9da8c2649f5ac5553de89121ae4e76e75d151880))

### Bug Fixes

- **deps:** update rust crate anyhow (1.0.103 → 1.0.104) ([#269](https://github.com/home-operations/kopiur/issues/269)) ([4d81496](https://github.com/home-operations/kopiur/commit/4d8149640ee59ec1676208920af4b257f5bea090))
- **deps:** update rust crate clap (4.6.1 → 4.6.2) ([#257](https://github.com/home-operations/kopiur/issues/257)) ([c238df1](https://github.com/home-operations/kopiur/commit/c238df13d1ac93158804875d9a1d98717395b382))
- **deps:** update rust crate futures (0.3.32 → 0.3.33) ([#268](https://github.com/home-operations/kopiur/issues/268)) ([926b52d](https://github.com/home-operations/kopiur/commit/926b52d3421ed39db8485252d3b6cea1bd8c0244))
- **deps:** update rust crate serde (1.0.228 → 1.0.229) ([#270](https://github.com/home-operations/kopiur/issues/270)) ([5fbd713](https://github.com/home-operations/kopiur/commit/5fbd71341c2fdd6a53bd6e3d8da4fbf1150a3caa))
- **deps:** update rust crate thiserror (2.0.18 → 2.0.19) ([#271](https://github.com/home-operations/kopiur/issues/271)) ([d83730f](https://github.com/home-operations/kopiur/commit/d83730f55aa805713cc50e60e77fbef1ae0be256))
- **deps:** update rust crate tokio (1.52.3 → 1.52.4) ([#261](https://github.com/home-operations/kopiur/issues/261)) ([accf6ae](https://github.com/home-operations/kopiur/commit/accf6ae98a37561ee1f4a8053acbe1f8d1d013db))
- writable backup source ([#254](https://github.com/home-operations/kopiur/issues/254)), stuck deletion finalizer ([#255](https://github.com/home-operations/kopiur/issues/255)), kopia epoch parameters ([#258](https://github.com/home-operations/kopiur/issues/258)) ([#263](https://github.com/home-operations/kopiur/issues/263)) ([94f9a15](https://github.com/home-operations/kopiur/commit/94f9a155ee2ffdbd5abd31b16393edd697faf6c4))

### Miscellaneous Chores

- **krew:** kopiur 0.7.5 manifest ([16d7390](https://github.com/home-operations/kopiur/commit/16d7390a5b188c647837c65ab7cd58ef2497de61))
- **mise:** Update tool aqua:astral-sh/uv (0.11.28 → 0.11.29) ([#256](https://github.com/home-operations/kopiur/issues/256)) ([f822335](https://github.com/home-operations/kopiur/commit/f822335ce16ed43e226a2868087a3592e726295d))
- **mise:** Update tool cosign (3.1.1 → 3.1.2) ([#267](https://github.com/home-operations/kopiur/issues/267)) ([649ba2e](https://github.com/home-operations/kopiur/commit/649ba2e59fe124f65c269f7e5cf6d4aebd452ac2))

## [0.7.5](https://github.com/home-operations/kopiur/compare/0.7.4...0.7.5) (2026-07-16)

### Bug Fixes

- **readme:** update readme admonition ([987957c](https://github.com/home-operations/kopiur/commit/987957c38b9ddfa1fe0ff75cdc3cac8d2d529181))
- **secctx:** SecurityContextCompatible is verified, not asserted; inherit merges with explicit ([#259](https://github.com/home-operations/kopiur/issues/259)) ([cbfd97d](https://github.com/home-operations/kopiur/commit/cbfd97dabb15c40807ef64c0ef4d51a3608da34e))

## [0.7.4](https://github.com/home-operations/kopiur/compare/0.7.3...0.7.4) (2026-07-15)

### Features

- bound Snapshot CR reconcile cost at scale ([#249](https://github.com/home-operations/kopiur/issues/249)) ([#253](https://github.com/home-operations/kopiur/issues/253)) ([3697aa9](https://github.com/home-operations/kopiur/commit/3697aa9ec618a40f32cae5d2f01a71ea050c0598))
- **docs:** clean up admonitions in docs ([71c5d85](https://github.com/home-operations/kopiur/commit/71c5d85068a8bfb468587275b12afc0f1c25db35))
- multi-cluster shared repository support — cluster identity, foreign-aware discovery, collision-free maintenance leases ([#251](https://github.com/home-operations/kopiur/issues/251)) ([a4593d8](https://github.com/home-operations/kopiur/commit/a4593d82049e6fd6a51903a788738c265bfe0f01))

### Bug Fixes

- batch of six open-issue fixes ([#248](https://github.com/home-operations/kopiur/issues/248) [#238](https://github.com/home-operations/kopiur/issues/238) [#245](https://github.com/home-operations/kopiur/issues/245) [#196](https://github.com/home-operations/kopiur/issues/196) [#237](https://github.com/home-operations/kopiur/issues/237) [#250](https://github.com/home-operations/kopiur/issues/250)) ([#252](https://github.com/home-operations/kopiur/issues/252)) ([045a5a5](https://github.com/home-operations/kopiur/commit/045a5a5038f4d5b2af28d4be5e15a004c6afde14))
- **deps:** update rust crate http-body-util (0.1.3 → 0.1.4) ([#244](https://github.com/home-operations/kopiur/issues/244)) ([4878006](https://github.com/home-operations/kopiur/commit/4878006e9750027792bef58e8ba22a0309a3bad4))
- **deps:** update rust crate rustls (0.23.41 → 0.23.42) ([#243](https://github.com/home-operations/kopiur/issues/243)) ([13b1dc4](https://github.com/home-operations/kopiur/commit/13b1dc4758a0401c54bcb2f577ff291af8c2620f))

### Miscellaneous Chores

- **krew:** kopiur 0.7.3 manifest ([d521675](https://github.com/home-operations/kopiur/commit/d5216755f2b1af9cf1e535fc929d4af9f63fee89))
- **mise:** Update tool aqua:EmbarkStudios/cargo-deny (0.19.9 → 0.20.2) ([#226](https://github.com/home-operations/kopiur/issues/226)) ([9c109b4](https://github.com/home-operations/kopiur/commit/9c109b4e4dc6be24b16249da17bdd1b38b62d8ad))
- **mise:** Update tool oxfmt (0.58.0 → 0.59.0) ([#246](https://github.com/home-operations/kopiur/issues/246)) ([41625e9](https://github.com/home-operations/kopiur/commit/41625e9487be3bf93d039559d5f5374c5af1e273))
- **mise:** Update tool zizmor (1.26.1 → 1.27.0) ([#247](https://github.com/home-operations/kopiur/issues/247)) ([fe10a6a](https://github.com/home-operations/kopiur/commit/fe10a6a74409a95adcff8c934698dcf4203ca234))

## [0.7.3](https://github.com/home-operations/kopiur/compare/0.7.2...0.7.3) (2026-07-12)

### Features

- add credential_projection to the cli ([#239](https://github.com/home-operations/kopiur/issues/239)) ([ffd551f](https://github.com/home-operations/kopiur/commit/ffd551f8da2349c3144abee68267f8ad685174c8))

### Bug Fixes

- bound projected credential Secrets to the mover Job, not the Snapshot CR ([#241](https://github.com/home-operations/kopiur/issues/241)) ([c035c2a](https://github.com/home-operations/kopiur/commit/c035c2a98885e3efcb920544c6e15893ae3d368d))
- populator no-op over bound PVCs, ClusterRepository secret namespace, stuck MaintenanceConfigured ([#236](https://github.com/home-operations/kopiur/issues/236)) ([b05c698](https://github.com/home-operations/kopiur/commit/b05c698dba8d2e876912bd2ac4d076a62e0e1ad2))

### Miscellaneous Chores

- **krew:** kopiur 0.7.2 manifest ([fd69b91](https://github.com/home-operations/kopiur/commit/fd69b91bec770477961d17de973ef9638d3943c0))

## [0.7.2](https://github.com/home-operations/kopiur/compare/0.7.1...0.7.2) (2026-07-11)

### Bug Fixes

- stable per-CR projected credential Secret names + legacy sweep ([#234](https://github.com/home-operations/kopiur/issues/234)) ([402945d](https://github.com/home-operations/kopiur/commit/402945dfd77e7b456c87eefa28e0c3ac48ea9b1e))

### Miscellaneous Chores

- **krew:** kopiur 0.7.1 manifest ([0d427d8](https://github.com/home-operations/kopiur/commit/0d427d8c6f769551e7538fac3d20ea08a16dac8f))

## [0.7.1](https://github.com/home-operations/kopiur/compare/0.7.0...0.7.1) (2026-07-10)

### Features

- expose kopia CLI tuning flags across the CRDs (closes [#216](https://github.com/home-operations/kopiur/issues/216)) ([#221](https://github.com/home-operations/kopiur/issues/221)) ([6c83418](https://github.com/home-operations/kopiur/commit/6c83418db7a4498cafe5c14d32b9b7761cf0ff41))
- staged-PVC storageClass/accessModes overrides + bind race fix (closes [#223](https://github.com/home-operations/kopiur/issues/223)) ([#228](https://github.com/home-operations/kopiur/issues/228)) ([416b68f](https://github.com/home-operations/kopiur/commit/416b68f55ea454b2646fb672a65854d0fd9d4d99))
- **tests:** also update e2e ([9a6a685](https://github.com/home-operations/kopiur/commit/9a6a685710f45a5132946b89e502abec0a10269a))

### Bug Fixes

- reap mover work-spec ConfigMaps at Job-terminal + orphan sweep ([#225](https://github.com/home-operations/kopiur/issues/225)) ([01c30a0](https://github.com/home-operations/kopiur/commit/01c30a04811ad98a9594bd07cd9393b6192bcc29))

### Miscellaneous Chores

- **krew:** kopiur 0.7.0 manifest ([3baf990](https://github.com/home-operations/kopiur/commit/3baf9908862d77fb4af2a7a170133071e9172241))
- **mise:** Update tool aqua:astral-sh/uv (0.11.27 → 0.11.28) ([#217](https://github.com/home-operations/kopiur/issues/217)) ([1d7d8b1](https://github.com/home-operations/kopiur/commit/1d7d8b1aeb34fd164ee33d9372a40d23e25909af))
- **mise:** Update tool helm (4.2.2 → 4.2.3) ([#227](https://github.com/home-operations/kopiur/issues/227)) ([b64f878](https://github.com/home-operations/kopiur/commit/b64f8788e8a885c9067590328a3dc011b79b7d6b))
- **mise:** Update tool lefthook (2.1.9 → 2.1.10) ([#218](https://github.com/home-operations/kopiur/issues/218)) ([7e3a79a](https://github.com/home-operations/kopiur/commit/7e3a79ac87780e07ef6c9cdc38155347bb5d6fd6))

## [0.7.0](https://github.com/home-operations/kopiur/compare/0.6.0...0.7.0) (2026-07-07)

### ⚠ BREAKING CHANGES

- **deps:** Update Rust crate croner (2.2.0 → 3.0.1) ([#45](https://github.com/home-operations/kopiur/issues/45))
- **controller:** inject replication destination credentials into the sync-to mover ([#200](https://github.com/home-operations/kopiur/issues/200)) (#207)
- **deps:** Update Rust crate kube (3.1.0 → 4.0.0) ([#124](https://github.com/home-operations/kopiur/issues/124))

### Features

- **deps:** update opentelemetry-rust monorepo (0.31.0 → 0.32.0) ([#169](https://github.com/home-operations/kopiur/issues/169)) ([355999f](https://github.com/home-operations/kopiur/commit/355999fd8379d9fde158b9a6376d38c958c5308e))
- **deps:** Update Rust crate croner (2.2.0 → 3.0.1) ([#45](https://github.com/home-operations/kopiur/issues/45)) ([0af8060](https://github.com/home-operations/kopiur/commit/0af806016a2b516695ea4cf7b30f3c885bbf8516))
- **deps:** Update Rust crate kube (3.1.0 → 4.0.0) ([#124](https://github.com/home-operations/kopiur/issues/124)) ([12d299c](https://github.com/home-operations/kopiur/commit/12d299cdb03c425c97cf66c57f950345cec08d52))
- **deps:** update rust crate rand (0.9.4 → 0.10.2) ([#102](https://github.com/home-operations/kopiur/issues/102)) ([b908d94](https://github.com/home-operations/kopiur/commit/b908d942a2c8e3f9aed18aee895dd85cb4585ce8))
- **deps:** update rust crate reqwest (0.12.28 → 0.13.4) ([#84](https://github.com/home-operations/kopiur/issues/84)) ([55cef3b](https://github.com/home-operations/kopiur/commit/55cef3bc3a41f28d7960819d473fd72ed3091fb4))

### Bug Fixes

- **controller:** inject replication destination credentials into the sync-to mover ([#200](https://github.com/home-operations/kopiur/issues/200)) ([#207](https://github.com/home-operations/kopiur/issues/207)) ([b787657](https://github.com/home-operations/kopiur/commit/b7876572e22a931d9f5119fef029702f4ad43423))
- **deps:** re-resolve Cargo.lock after concurrent dependency merges ([c8765f6](https://github.com/home-operations/kopiur/commit/c8765f6c8b252b55955363bb83661937452a7f48))

### Documentation

- credit GoReleaser Pro in the README ([#212](https://github.com/home-operations/kopiur/issues/212)) ([620bc88](https://github.com/home-operations/kopiur/commit/620bc88b0d7d67404ce168a7dbf649d7dd8e52f5))
- **upgrade:** add 0.5.x → 0.6.0 CRD migration guide ([#209](https://github.com/home-operations/kopiur/issues/209)) ([603eddb](https://github.com/home-operations/kopiur/commit/603eddbb91ec32eaf8bde9723daaf5097bd9cfff))
- **upgrade:** further clarify exactly how to upgrade from 0.5.x -&gt; 0.6.x ([cf4ea98](https://github.com/home-operations/kopiur/commit/cf4ea986f44f5c8d5a9428fea0ffe04414c356c4))
- **upgrade:** make the steps explicit, fix the CRD selector, cover Flux ([eac6fff](https://github.com/home-operations/kopiur/commit/eac6fff3fe914f92a1b315e6994661fefa9ddac0))

### Miscellaneous Chores

- **deps:** lock file maintenance ([#186](https://github.com/home-operations/kopiur/issues/186)) ([7290972](https://github.com/home-operations/kopiur/commit/7290972dd3f61ed6b8cc2b137abcac2374ee8467))
- **krew:** kopiur 0.6.0 manifest ([c3cd1ed](https://github.com/home-operations/kopiur/commit/c3cd1ed80c9f2c56041f8d4f56b54d6f8ffc7e5b))
- **mise:** Update tool aqua:astral-sh/uv (0.11.26 → 0.11.27) ([#211](https://github.com/home-operations/kopiur/issues/211)) ([da2ab78](https://github.com/home-operations/kopiur/commit/da2ab78962753c9e765e58c0a0c44dcb5b231346))
- **mise:** Update tool oxfmt (0.57.0 → 0.58.0) ([#213](https://github.com/home-operations/kopiur/issues/213)) ([93486ab](https://github.com/home-operations/kopiur/commit/93486ab88bd59efaecf66c25f0ea852725382003))
- **mise:** Update tool rust (1.95.0 → 1.96.1) ([#180](https://github.com/home-operations/kopiur/issues/180)) ([02b7aeb](https://github.com/home-operations/kopiur/commit/02b7aeb78aa26c2e0ffc0cc35477e77fe832d55c))

## [0.6.0](https://github.com/home-operations/kopiur/compare/0.5.2...0.6.0) (2026-07-06)

### ⚠ BREAKING CHANGES

- **chart:** regroup values to the org operator-chart shape ([#203](https://github.com/home-operations/kopiur/issues/203)) (#206)

### Features

- **chart:** regroup values to the org operator-chart shape ([#203](https://github.com/home-operations/kopiur/issues/203)) ([#206](https://github.com/home-operations/kopiur/issues/206)) ([abe07d6](https://github.com/home-operations/kopiur/commit/abe07d65df62de99835e3b0ef8b1bef114cb2532))
- **config:** clap flags with env fallback for controller, webhook and mover ([#204](https://github.com/home-operations/kopiur/issues/204)) ([7524f75](https://github.com/home-operations/kopiur/commit/7524f753f55131b218f5822397776ecf1c0aca9f))
- **docs:** generate the CRD field reference; emit hidden schema defaults; dual-stack binds; Helm env/probes ([#205](https://github.com/home-operations/kopiur/issues/205)) ([2de7030](https://github.com/home-operations/kopiur/commit/2de703084fcd41f87afc3ef05e5640bd7ca016a7))

### Miscellaneous Chores

- **krew:** kopiur 0.5.2 manifest ([6d2a155](https://github.com/home-operations/kopiur/commit/6d2a155ca6d068ecffcf843fadf8c10c3a760028))

## [0.5.2](https://github.com/home-operations/kopiur/compare/0.5.1...0.5.2) (2026-07-04)

### Features

- **docs:** update docs for some restores and tests ([a3e3850](https://github.com/home-operations/kopiur/commit/a3e38506d7c1b658b990e0206c2b38137ee8ca5e))

### Bug Fixes

- **controller:** transient VolumeSnapshot errors no longer terminally fail backups ([#201](https://github.com/home-operations/kopiur/issues/201)) ([757793f](https://github.com/home-operations/kopiur/commit/757793f9aefca6e669da1a44a6553af67e36af3d))

### Miscellaneous Chores

- **krew:** kopiur 0.5.1 manifest ([afb3ab2](https://github.com/home-operations/kopiur/commit/afb3ab2f8f9e118d140e75437e6b0f1c18a43b30))

## [0.5.1](https://github.com/home-operations/kopiur/compare/0.5.0...0.5.1) (2026-07-04)

### Features

- **cli:** release the kubectl plugin via goreleaser (Homebrew cask + krew) ([#187](https://github.com/home-operations/kopiur/issues/187)) ([6aa0080](https://github.com/home-operations/kopiur/commit/6aa008057677c32316efe19684789b0df43868f8))

### Miscellaneous Chores

- **krew:** kopiur 0.5.0 manifest ([1fab014](https://github.com/home-operations/kopiur/commit/1fab0143d1ff40d40c9411bdcca2cd2e03918ddf))

## [0.5.0](https://github.com/home-operations/kopiur/compare/0.4.13...0.5.0) (2026-07-04)

### ⚠ BREAKING CHANGES

- **api:** gate verification on a verifiable snapshot; nest quick under schedule ([#191](https://github.com/home-operations/kopiur/issues/191))
- **api:** copyMethod defaults to Snapshot; repo-level timezone defaults; KOPIUR_HTTP_ADDR ([#192](https://github.com/home-operations/kopiur/issues/192))
- **observability:** store-backed metrics — series live and die with their CRs ([#190](https://github.com/home-operations/kopiur/issues/190))

### Features

- **api:** copyMethod defaults to Snapshot; repo-level timezone defaults; KOPIUR_HTTP_ADDR ([#192](https://github.com/home-operations/kopiur/issues/192)) ([e1291d0](https://github.com/home-operations/kopiur/commit/e1291d057ca8872763d8f4c811f956dd613d4754))
- **api:** default files.ignoreRules to OS-artifact excludes (lost+found and friends) ([#193](https://github.com/home-operations/kopiur/issues/193)) ([0674b3b](https://github.com/home-operations/kopiur/commit/0674b3be797b5b7f45b1c09d6660f9ce6d809718))
- **api:** gate verification on a verifiable snapshot; nest quick under schedule ([#191](https://github.com/home-operations/kopiur/issues/191)) ([9bdc9a7](https://github.com/home-operations/kopiur/commit/9bdc9a70722bf70a5c4335fb11731128a19bc941))
- **deps:** update rust crate cel (0.13.0 → 0.14.0) ([#178](https://github.com/home-operations/kopiur/issues/178)) ([d98718c](https://github.com/home-operations/kopiur/commit/d98718c12d1c01827a99e367ca921f59a95daed5))

### Bug Fixes

- **deps:** update rust crate anyhow (1.0.102 → 1.0.103) ([#170](https://github.com/home-operations/kopiur/issues/170)) ([41257b8](https://github.com/home-operations/kopiur/commit/41257b89ce1857421918076493932a7bd3aeb587))
- **deps:** update rust crate rustls (0.23.40 → 0.23.41) ([#160](https://github.com/home-operations/kopiur/issues/160)) ([ebe8263](https://github.com/home-operations/kopiur/commit/ebe8263685cc30b6c020ed4110ec49bf90a6b36c))
- **deps:** update rust crate time (0.3.49 → 0.3.51) ([#157](https://github.com/home-operations/kopiur/issues/157)) ([393b58d](https://github.com/home-operations/kopiur/commit/393b58d50afb1a7489c004f0fc257bea61a59831))
- **observability:** store-backed metrics — series live and die with their CRs ([#190](https://github.com/home-operations/kopiur/issues/190)) ([576e457](https://github.com/home-operations/kopiur/commit/576e45773ed7e49673c0b26e49bff6029736ed78))

### Miscellaneous Chores

- **krew:** kopiur 0.4.13 manifest ([9c090a9](https://github.com/home-operations/kopiur/commit/9c090a996c227bee8dd108b7f99b57c6c3f9a933))
- **mise:** Update tool aqua:astral-sh/uv (0.11.21 → 0.11.24) ([#133](https://github.com/home-operations/kopiur/issues/133)) ([bacc9f8](https://github.com/home-operations/kopiur/commit/bacc9f856a60078f4caec62947e3068320a0c024))
- **mise:** Update tool aqua:astral-sh/uv (0.11.24 → 0.11.25) ([#173](https://github.com/home-operations/kopiur/issues/173)) ([183247e](https://github.com/home-operations/kopiur/commit/183247eb701d2a1c1cd6440548b309fc2e4a1125))
- **mise:** Update tool aqua:astral-sh/uv (0.11.25 → 0.11.26) ([#185](https://github.com/home-operations/kopiur/issues/185)) ([87c3bcb](https://github.com/home-operations/kopiur/commit/87c3bcb6cfd74de5e4300f83ff66e4905fdcada7))
- **mise:** Update tool node (22.22.3 → v24.18.0) ([#158](https://github.com/home-operations/kopiur/issues/158)) ([d0e0398](https://github.com/home-operations/kopiur/commit/d0e0398be49f1ec6f46ddc70fc56fad2c13aa9f9))
- **mise:** Update tool oxfmt (0.55.0 → 0.56.0) ([#161](https://github.com/home-operations/kopiur/issues/161)) ([e0c93c3](https://github.com/home-operations/kopiur/commit/e0c93c3f81ab50f43eb4a0b69a1a6698b103eb50))
- **mise:** Update tool oxfmt (0.56.0 → 0.57.0) ([#183](https://github.com/home-operations/kopiur/issues/183)) ([a1f0629](https://github.com/home-operations/kopiur/commit/a1f0629490d7b7d3f078ed5c78a85c69efaf4859))
- **renovate:** drop the dead cargo postUpgradeTask ([#177](https://github.com/home-operations/kopiur/issues/177)) ([83eba29](https://github.com/home-operations/kopiur/commit/83eba2949019ace1ecda459e64591f7b4baaac8b))
- **renovate:** inherit shared chart-docs postUpgradeTasks preset ([#179](https://github.com/home-operations/kopiur/issues/179)) ([2c614f9](https://github.com/home-operations/kopiur/commit/2c614f991d4ff3fc8bee6b1645eb6c64859ec8df))
- test out regenerating cargo lock on update ([55daa93](https://github.com/home-operations/kopiur/commit/55daa9385316cb86fb987d351626a0e03b20f323))

## [0.4.13](https://github.com/home-operations/kopiur/compare/0.4.12...0.4.13) (2026-06-24)

### Features

- native gdrive backend, rclone/bootstrap timeouts, CRD cleanups ([#152](https://github.com/home-operations/kopiur/issues/152), [#154](https://github.com/home-operations/kopiur/issues/154), [#155](https://github.com/home-operations/kopiur/issues/155)) ([#164](https://github.com/home-operations/kopiur/issues/164)) ([03b336c](https://github.com/home-operations/kopiur/commit/03b336c1b4714da629c222ba9dc403c980b4f102))
- **probe:** support repo health checks ([#165](https://github.com/home-operations/kopiur/issues/165)) ([d5b9eb2](https://github.com/home-operations/kopiur/commit/d5b9eb2eafa97e6cbed43bf2ac23777651e30acf))

### Bug Fixes

- **mise:** hopefully make the mise lock happy now ([d230ac7](https://github.com/home-operations/kopiur/commit/d230ac7e0bd59f6203aa6caa5d88f721f80a9728))
- **mover:** drop phase from the progress heartbeat (Restore "Running" 422) ([#163](https://github.com/home-operations/kopiur/issues/163)) ([84cdc6c](https://github.com/home-operations/kopiur/commit/84cdc6ce687ae52f731da2dde4de3a5a9500dc94))

### Miscellaneous Chores

- **krew:** kopiur 0.4.12 manifest ([2117b17](https://github.com/home-operations/kopiur/commit/2117b172613b8bcb2254198467df371a7c6fb1f0))

## [0.4.12](https://github.com/home-operations/kopiur/compare/0.4.11...0.4.12) (2026-06-22)

### Features

- **restore:** resolve "latest" for object-store backends in the mover Job ([#153](https://github.com/home-operations/kopiur/issues/153)) ([29fb2a4](https://github.com/home-operations/kopiur/commit/29fb2a4dc19c808f284dd5391da5fa19ef991718))

### Miscellaneous Chores

- **krew:** kopiur 0.4.11 manifest ([03c71d5](https://github.com/home-operations/kopiur/commit/03c71d516f84c490795502dff647cbf42d338330))
- **mise:** Update tool zizmor (1.25.2 → 1.26.1) ([#151](https://github.com/home-operations/kopiur/issues/151)) ([a3536c5](https://github.com/home-operations/kopiur/commit/a3536c54987c6330584560808ab3e7879a5023be))

## [0.4.11](https://github.com/home-operations/kopiur/compare/0.4.10...0.4.11) (2026-06-21)

### Features

- **dev:** oxfmt doesn't play nicely on fresh repo setup ([2907ed5](https://github.com/home-operations/kopiur/commit/2907ed50e65b04b7fd9522cfb0a9be131c59b187))
- repository health fail-fast, cron timezones, and verification successExpr fixes ([#150](https://github.com/home-operations/kopiur/issues/150)) ([9068373](https://github.com/home-operations/kopiur/commit/90683739162f491011fc3272dbf1b08981b31bcb))

### Miscellaneous Chores

- **krew:** kopiur 0.4.10 manifest ([5616213](https://github.com/home-operations/kopiur/commit/56162139235910bfc456d34f5139aea29a9214c0))

## [0.4.10](https://github.com/home-operations/kopiur/compare/0.4.9...0.4.10) (2026-06-20)

### Features

- **crds:** eh, validate some more schemas ([d59e6aa](https://github.com/home-operations/kopiur/commit/d59e6aa04dfe09b03a5a990704fac641b628c06a))
- **crds:** trim CRD descriptions to one sentence; move detail to docs ([#146](https://github.com/home-operations/kopiur/issues/146)) ([2ba82a6](https://github.com/home-operations/kopiur/commit/2ba82a6ed95e588cb1328da3ece27f054d7a2430))
- **crds:** try to slim up the CRDs some more ([990fff4](https://github.com/home-operations/kopiur/commit/990fff4c895ea2d32400b6c50e727f877add060e))
- **identity:** validate identity shape + guard re-identification on edit ([#141](https://github.com/home-operations/kopiur/issues/141)) ([2509680](https://github.com/home-operations/kopiur/commit/2509680ec5e1d305a947d8a5351402b629aecde7))
- **migrate:** offline/GitOps mode for `migrate volsync` ([#143](https://github.com/home-operations/kopiur/issues/143)) ([605df1f](https://github.com/home-operations/kopiur/commit/605df1f5a272bfcde2edb791b0cc0970c2fb5b0c))
- **scratch:** inherit deep-verify scratch defaults from moverDefaults.scratch ([#139](https://github.com/home-operations/kopiur/issues/139)) ([521f3b1](https://github.com/home-operations/kopiur/commit/521f3b13cc7435a6d04361d410a208dae4f2868b))

### Bug Fixes

- **restore:** provision empty volume on onMissingSnapshot=Continue ([#144](https://github.com/home-operations/kopiur/issues/144)) ([bc0d2ee](https://github.com/home-operations/kopiur/commit/bc0d2ee8492e4a7bf7bb4fafec916c1f9e38cf2b))
- **restore:** re-resolve kopia snapshot id after pin; heal stale ids ([#137](https://github.com/home-operations/kopiur/issues/137)) ([#142](https://github.com/home-operations/kopiur/issues/142)) ([c17dace](https://github.com/home-operations/kopiur/commit/c17dace26c0aac5ef5ea7aa7ff96cf5f1785cfd1))

### Miscellaneous Chores

- **krew:** kopiur 0.4.9 manifest ([399a346](https://github.com/home-operations/kopiur/commit/399a346572cd089b25ba4120720436cf37b7318f))

### Code Refactoring

- split oversized files into modules + extract kopiur-migrate crate ([#145](https://github.com/home-operations/kopiur/issues/145)) ([1ed484d](https://github.com/home-operations/kopiur/commit/1ed484daf3e0e0f8882f2787de161e59b0141c23))

## [0.4.9](https://github.com/home-operations/kopiur/compare/0.4.8...0.4.9) (2026-06-19)

### Features

- **docs:** add some more useful examples and documentation as well ([391598c](https://github.com/home-operations/kopiur/commit/391598c86b295c9ae3bce473b4f45f5063317749))
- **docs:** also update docs to remove ADR references ([90d80ae](https://github.com/home-operations/kopiur/commit/90d80ae6041c14572b707adb81ae978284783cb4))
- **docs:** update docs to avoid using `kubectl` everywhere ([a19d0d5](https://github.com/home-operations/kopiur/commit/a19d0d5bdcf47089b4138af1a5565552247a5f9c))
- **helm:** let's...not set CPU or MEM limits on resources for now ([5ae3608](https://github.com/home-operations/kopiur/commit/5ae3608e338623132eb4f034ec30aa13b933ed1e))
- move the controller monitoring port (metrics + health) to 8081 ([#130](https://github.com/home-operations/kopiur/issues/130)) ([6ef0b3f](https://github.com/home-operations/kopiur/commit/6ef0b3f708b26a0a0765ea43c0ff6242f2716166))
- **rbac:** update RBAC settings on the Helm Chart ([#131](https://github.com/home-operations/kopiur/issues/131)) ([0d5d0ee](https://github.com/home-operations/kopiur/commit/0d5d0ee58b2657dfbffc73729e058f50b52edcc8))
- **scratch:** mount an ephemeral pvc for use of the scratch dir ([#132](https://github.com/home-operations/kopiur/issues/132)) ([4e586a7](https://github.com/home-operations/kopiur/commit/4e586a78db3505137f06421f7e1fa9588300ab73))
- **secctx:** validate mover↔workload securityContext compatibility ([#134](https://github.com/home-operations/kopiur/issues/134)) ([6e76716](https://github.com/home-operations/kopiur/commit/6e767160e8bf283acb969a6ac1e94f0ada993323))

### Documentation

- **secctx:** reference + troubleshooting for the securityContext-compat feature ([#136](https://github.com/home-operations/kopiur/issues/136)) ([0ab8457](https://github.com/home-operations/kopiur/commit/0ab8457584c5bfac4d31a62afe5ecddfdad67851))

### Miscellaneous Chores

- **krew:** kopiur 0.4.8 manifest ([4ac97a0](https://github.com/home-operations/kopiur/commit/4ac97a088d743f5cd9c5f86a1acf037134c5f9eb))
- **mise:** update tool aqua:embarkstudios/cargo-deny (0.19.8 → 0.19.9) ([#118](https://github.com/home-operations/kopiur/issues/118)) ([ed66f18](https://github.com/home-operations/kopiur/commit/ed66f182637f15101b36c22c3b73ff992ba8e555))
- **mise:** update tool helm (4.2.1 → 4.2.2) ([#127](https://github.com/home-operations/kopiur/issues/127)) ([16f6128](https://github.com/home-operations/kopiur/commit/16f6128f9d8e9b6fef259cb5f95f8cc983794ff0))

### Code Refactoring

- **chart:** rename templates to .tpl ([#129](https://github.com/home-operations/kopiur/issues/129)) ([9baba0b](https://github.com/home-operations/kopiur/commit/9baba0bca11e2b2e455155b80548d8b607d02718))

## [0.4.8](https://github.com/home-operations/kopiur/compare/0.4.7...0.4.8) (2026-06-16)

### Features

- **repository:** warn on too-many-index-blobs + self-heal stale maintenance owner ([#122](https://github.com/home-operations/kopiur/issues/122)) ([2365249](https://github.com/home-operations/kopiur/commit/2365249e3fa0152153026c852aeb50df35d40fc2))

### Bug Fixes

- **docker:** tolerate a v-prefixed KOPIA_VERSION so image builds don't 404 ([5cf2df2](https://github.com/home-operations/kopiur/commit/5cf2df2ea850c17c6ca150976faaca3526bffad0))
- **restore:** complete the populator rebind when the mover stamps Completed early ([#121](https://github.com/home-operations/kopiur/issues/121)) ([19b6412](https://github.com/home-operations/kopiur/commit/19b6412328a2a204058d96fc40e6ac450b4df3a9))

### Miscellaneous Chores

- **krew:** kopiur 0.4.7 manifest ([01c6234](https://github.com/home-operations/kopiur/commit/01c62346dad1e969c4c083da4216c382cc565cb7))
- **mise:** update tool kopia (0.23.0 → v0.23.1) ([#120](https://github.com/home-operations/kopiur/issues/120)) ([88edf8c](https://github.com/home-operations/kopiur/commit/88edf8cfba29f9b04bbf4dfb45c8fae48f41e2c6))
- **mise:** update tool oxfmt (0.54.0 → 0.55.0) ([#119](https://github.com/home-operations/kopiur/issues/119)) ([5891810](https://github.com/home-operations/kopiur/commit/5891810983e8e6d3ece921aa31f4281c707c4764))

## [0.4.7](https://github.com/home-operations/kopiur/compare/0.4.6...0.4.7) (2026-06-15)

### Features

- **mover:** add some tests for the podStartupDeadlineSeconds ([a7daba9](https://github.com/home-operations/kopiur/commit/a7daba99b55e25d757e99716445986cbbc6706a4))
- **mover:** support and create podStartupDeadlineSeconds ([3b6b362](https://github.com/home-operations/kopiur/commit/3b6b362c7140228c50e44d3f78ff1348d412f1d5))
- **restore:** implement target.populator volume-populator handshake ([#117](https://github.com/home-operations/kopiur/issues/117)) ([e7aba93](https://github.com/home-operations/kopiur/commit/e7aba939bb66bc0e252065bc4a6b058cc9dbcc6f))
- **tests:** I love having to fix this NFS test ([27f1e2f](https://github.com/home-operations/kopiur/commit/27f1e2f2bb4decdff67f53c79c4897fde543e11b))
- **tests:** maybe finally fix the NFS E2E tests? ([2be6717](https://github.com/home-operations/kopiur/commit/2be6717496dcca52ec3f543f7ac38c8f2435cfec))
- **tests:** update e2e tests to also cleanup kind ([ba44e10](https://github.com/home-operations/kopiur/commit/ba44e10eadac31dfca620198f1f74389ee979a8e))

### Bug Fixes

- **dev:** update mise e2e commands ([458f688](https://github.com/home-operations/kopiur/commit/458f688246800e3602ee73a494000c8b78d5bac9))
- **tests:** also cleanup tests ([3de8beb](https://github.com/home-operations/kopiur/commit/3de8beb3f042fc44b494a2224e165270a266fc33))

### Miscellaneous Chores

- **krew:** kopiur 0.4.6 manifest ([66e03e5](https://github.com/home-operations/kopiur/commit/66e03e5149ff782da5e1fd10b86ce25f6716f6d7))

## [0.4.6](https://github.com/home-operations/kopiur/compare/0.4.5...0.4.6) (2026-06-15)

### Features

- **nfs:** support podsecuritycontext for server too ([#109](https://github.com/home-operations/kopiur/issues/109)) ([ed2fe8b](https://github.com/home-operations/kopiur/commit/ed2fe8be0342fef7ea7101b49253e2b2b60905a4))
- **server:** support read-only server ([#108](https://github.com/home-operations/kopiur/issues/108)) ([e317480](https://github.com/home-operations/kopiur/commit/e31748045a60abedebc3ca78de29c7590e36a1fe))

### Bug Fixes

- **e2e:** isolate NFS consumers from the group-restricted grouprepo subdir ([167e2ec](https://github.com/home-operations/kopiur/commit/167e2ec3ba44b4a331f41d5745f616af097916ce))

### Miscellaneous Chores

- **krew:** kopiur 0.4.5 manifest ([b9455a2](https://github.com/home-operations/kopiur/commit/b9455a2ce39cf85d9efbf6cfecabf25a1bab7ae5))

## [0.4.5](https://github.com/home-operations/kopiur/compare/0.4.4...0.4.5) (2026-06-14)

### Features

- **observability:** refresh dashboard + implement snapshot verified-timestamp metric ([#106](https://github.com/home-operations/kopiur/issues/106)) ([4b962d1](https://github.com/home-operations/kopiur/commit/4b962d182c844f6954fb0569c7129ccd92911266))

### Bug Fixes

- **deps:** update rust crate time (0.3.47 → 0.3.49) ([#94](https://github.com/home-operations/kopiur/issues/94)) ([c62518a](https://github.com/home-operations/kopiur/commit/c62518abc2e6433368f702fa13415f15c7fdec11))

### Miscellaneous Chores

- **krew:** kopiur 0.4.4 manifest ([226f7b9](https://github.com/home-operations/kopiur/commit/226f7b971657dbddc31878bed683144b9080adb2))
- **mise:** update tool aqua:astral-sh/uv (0.9.9 → 0.11.21) ([#62](https://github.com/home-operations/kopiur/issues/62)) ([d537044](https://github.com/home-operations/kopiur/commit/d537044712160ed8d39a32dca8dde3b233dcfdb2))
- **mise:** update tool helm (4.2.0 → 4.2.1) ([#96](https://github.com/home-operations/kopiur/issues/96)) ([63c7df2](https://github.com/home-operations/kopiur/commit/63c7df28fc206a7c733e5c945a530267b8d458ca))
- **mise:** update tool kubectl (1.36.1 → 1.36.2) ([#95](https://github.com/home-operations/kopiur/issues/95)) ([e055a1e](https://github.com/home-operations/kopiur/commit/e055a1ef86cfa51ad0810a1bf7e0b71f8c46e3b3))

## [0.4.4](https://github.com/home-operations/kopiur/compare/0.4.3...0.4.4) (2026-06-14)

### Bug Fixes

- **controller:** don't reap staged source until the mover Job is terminal ([#103](https://github.com/home-operations/kopiur/issues/103)) ([#104](https://github.com/home-operations/kopiur/issues/104)) ([fed3344](https://github.com/home-operations/kopiur/commit/fed3344c6f0bfad4d45d682c062ffe55a9694dda))

### Miscellaneous Chores

- **krew:** kopiur 0.4.3 manifest ([561587b](https://github.com/home-operations/kopiur/commit/561587b8a5c5cd95a873d59ef5e48a2b83d5c670))

## [0.4.3](https://github.com/home-operations/kopiur/compare/0.4.2...0.4.3) (2026-06-14)

### Features

- **ui:** implement kopia ui...for now ([#100](https://github.com/home-operations/kopiur/issues/100)) ([f70eda5](https://github.com/home-operations/kopiur/commit/f70eda5782d88e7f396f7a5c4dc80427dba0cb39))

### Miscellaneous Chores

- **krew:** kopiur 0.4.2 manifest ([89ec6da](https://github.com/home-operations/kopiur/commit/89ec6da4204b5405962fc0a7f1b357ff455a0a78))

## [0.4.2](https://github.com/home-operations/kopiur/compare/0.4.1...0.4.2) (2026-06-13)

### Bug Fixes

- **docs:** also reorg docs ([659c5ac](https://github.com/home-operations/kopiur/commit/659c5acf028afae7559958ced7e9d248b12b5a25))

### Performance Improvements

- **controller:** cut RAM — mimalloc, capped worker pool, scoped/metadata watches ([aa55b0d](https://github.com/home-operations/kopiur/commit/aa55b0d5ab2202dcfa039085e4d3a267df02bdd4))

### Miscellaneous Chores

- **krew:** kopiur 0.4.1 manifest ([fe19db7](https://github.com/home-operations/kopiur/commit/fe19db770d1c35b15edf42154e5cf2c5f522ecd7))

## [0.4.1](https://github.com/home-operations/kopiur/compare/0.4.0...0.4.1) (2026-06-13)

### Features

- **credentials:** support the use of a cloud object-store ([776aa3a](https://github.com/home-operations/kopiur/commit/776aa3af8667cbab3798b84b396abbb79187366b))
- **docs:** add a ton more docs too for walkthroughs ([0e2f134](https://github.com/home-operations/kopiur/commit/0e2f134efa606546814c0e3a74f0e6de981d5b77))
- **krew:** support more volsync kopia migration using CLI ([24a9bf2](https://github.com/home-operations/kopiur/commit/24a9bf20e22e6390cba7cb18c8d9aacf858531a1))
- **tests:** also update e2e ([3b2447e](https://github.com/home-operations/kopiur/commit/3b2447ead2323d8d71f849f40b72a8e5498c68e7))

### Bug Fixes

- **catalog:** scan on spec change + preserve mover snapshot id in the Ready heal ([064d11a](https://github.com/home-operations/kopiur/commit/064d11aaa8fcad2d67a315206b0d9365bf7ba44c))
- **dev:** resolve issue with doc build script ([762af2e](https://github.com/home-operations/kopiur/commit/762af2ec161966e5e532499f18dd5f7d0140452d))
- **reconcile:** continue to not cook CPUs ([0c1e8a7](https://github.com/home-operations/kopiur/commit/0c1e8a72d1df3cd4061dbe6c41c6c6a621265770))
- **reconcile:** shrink controller debounce so terminal heal isn't sta… ([#98](https://github.com/home-operations/kopiur/issues/98)) ([b4241aa](https://github.com/home-operations/kopiur/commit/b4241aae0f711ff6c422cd39c92678ef6f86d250))
- **reconcile:** try to stop making clusters toasty ([de0ba00](https://github.com/home-operations/kopiur/commit/de0ba003f5194bb85e923bc0d165681335b05330))
- **restore:** kstatus Ready conditions on every phase transition + mover-stamp heal ([a43af02](https://github.com/home-operations/kopiur/commit/a43af02f99564a935ee012a4d510c3a6152c4e24))

### Miscellaneous Chores

- **krew:** kopiur 0.4.0 manifest ([182a456](https://github.com/home-operations/kopiur/commit/182a45631b538f292e3e139780e8fe8208bb8062))

## [0.4.0](https://github.com/home-operations/kopiur/compare/0.3.5...0.4.0) (2026-06-12)

### ⚠ BREAKING CHANGES

- **github-action:** Update action codecov/codecov-action (v6.0.2 → v7.0.0) ([#61](https://github.com/home-operations/kopiur/issues/61))

### Features

- **bootstrap:** of course explicitly own the bootstrap job as well ([7edb295](https://github.com/home-operations/kopiur/commit/7edb2957935e20034372b7249f09edddb715b717))
- **deps:** update rust crate insta (1.47.2 → 1.48.0) ([#87](https://github.com/home-operations/kopiur/issues/87)) ([592b892](https://github.com/home-operations/kopiur/commit/592b89220b187221d719cdd248603f585055cf80))
- **reconcile:** resolve issue where resources are reconciled outside of their window ([1d7a82b](https://github.com/home-operations/kopiur/commit/1d7a82b9429345bcbd2dd6d365d54a45001cd304))
- **src:** add some more documentation around e2e and cli ([d4cbe9c](https://github.com/home-operations/kopiur/commit/d4cbe9c847e281a7c30f8cc8f55f42fdc818f377))

### Bug Fixes

- **deps:** update rust crate http (1.4.1 → 1.4.2) ([#69](https://github.com/home-operations/kopiur/issues/69)) ([f7ac524](https://github.com/home-operations/kopiur/commit/f7ac524ab3a2099df33ab41c6d953fe501e74a65))
- **dev:** try to fix cargo lock ([f093a05](https://github.com/home-operations/kopiur/commit/f093a05910a8df28f67fb574a54eb3e92ae5ce72))

### Miscellaneous Chores

- **deps:** lock file maintenance ([#55](https://github.com/home-operations/kopiur/issues/55)) ([15e6338](https://github.com/home-operations/kopiur/commit/15e6338187da320ab6842b54b65ccc7fd975e1bb))
- **krew:** kopiur 0.3.5 manifest ([be4db12](https://github.com/home-operations/kopiur/commit/be4db1271da71015675186675267187074bebac3))

### Continuous Integration

- **github-action:** Update action codecov/codecov-action (v6.0.2 → v7.0.0) ([#61](https://github.com/home-operations/kopiur/issues/61)) ([4629018](https://github.com/home-operations/kopiur/commit/4629018619acb0d4b94e56b3f9893e0d9d105e9d))

## [0.3.5](https://github.com/home-operations/kopiur/compare/0.3.4...0.3.5) (2026-06-12)

### Features

- **ci:** append a `v` so that krew is happy ([6455a54](https://github.com/home-operations/kopiur/commit/6455a54dbdbf6ac395956627deb31637a23b4be5))

### Miscellaneous Chores

- **krew:** kopiur 0.3.4 manifest ([9ee83e5](https://github.com/home-operations/kopiur/commit/9ee83e57971c4a40abb0a5f93ee0aeb81790e981))

## [0.3.4](https://github.com/home-operations/kopiur/compare/0.3.3...0.3.4) (2026-06-12)

### Features

- **docs:** add docs about rwop ([56f6381](https://github.com/home-operations/kopiur/commit/56f63818b6c6381e7dc0928023af5deb15ca03c5))

## [0.3.3](https://github.com/home-operations/kopiur/compare/0.3.2...0.3.3) (2026-06-11)

### Features

- **csi:** add csi snapshots ([#89](https://github.com/home-operations/kopiur/issues/89)) ([2bf8626](https://github.com/home-operations/kopiur/commit/2bf862607c21c24aad4c21bfd2ca4042c1bbae60))
- **mover:** support rwo and pvcs tied to a node ([#85](https://github.com/home-operations/kopiur/issues/85)) ([8926f94](https://github.com/home-operations/kopiur/commit/8926f946f4c2287dcb7fe4df42be083a5b65206d))

### Bug Fixes

- **readme:** update readme to clarify still under heavy dev ([becf173](https://github.com/home-operations/kopiur/commit/becf173fa404028794987864e7f2667bdf0b97fb))

## [0.3.2](https://github.com/home-operations/kopiur/compare/0.3.1...0.3.2) (2026-06-11)

### Features

- **docs:** add a ton of docs and comments ([0197546](https://github.com/home-operations/kopiur/commit/01975467a8bae81c21756a23ea3e854048a686de))
- **license:** uhhh idk ([cc78085](https://github.com/home-operations/kopiur/commit/cc78085b04cd45c64f5ef33bf69c59ccd6396b34))

## [0.3.1](https://github.com/home-operations/kopiur/compare/0.3.0...0.3.1) (2026-06-10)

### Bug Fixes

- **mover:** update mover default perms ([c652dac](https://github.com/home-operations/kopiur/commit/c652dacca61c4e45a9b05c24eab5662790584c0b))

## [0.3.0](https://github.com/home-operations/kopiur/compare/0.2.0...0.3.0) (2026-06-10)

### ⚠ BREAKING CHANGES

- **github-action:** Update GitHub Artifact Actions (v7.0.0 → v8.0.1) ([#68](https://github.com/home-operations/kopiur/issues/68))

### Features

- **github-release:** update release helm-unittest/helm-unittest (v1.0.3 → v1.1.1) ([#67](https://github.com/home-operations/kopiur/issues/67)) ([59b0f94](https://github.com/home-operations/kopiur/commit/59b0f94fb99f928f5de7c1412a56a95b9df9dbc4))
- **mise:** update tool cosign (3.0.6 → 3.1.1) ([#76](https://github.com/home-operations/kopiur/issues/76)) ([0d04e85](https://github.com/home-operations/kopiur/commit/0d04e856addecb66a51c836d79eb2fca33bed332))
- **mise:** update tool oxfmt (0.53.0 → 0.54.0) ([#70](https://github.com/home-operations/kopiur/issues/70)) ([07d528d](https://github.com/home-operations/kopiur/commit/07d528d2668ed157fa80095be453e5f22c83ceef))
- **repository:** don't stunlock on encryption setting ([0357aa1](https://github.com/home-operations/kopiur/commit/0357aa1929af78ca0902d60f011ed05939a4b8d1))
- **repository:** if a repo already exists, and we have correct creds - import state ([88a1719](https://github.com/home-operations/kopiur/commit/88a1719dfabd6ee3d3a3db82f5409a4fbc6ee2de))
- **ux:** make sure to not make fields immutable that don't need to be ([f1772b5](https://github.com/home-operations/kopiur/commit/f1772b5fcb70e1f7a0f222d5648ba2a920149e3b))

### Bug Fixes

- **helm:** update the name of `crds.yaml` ([15e79ed](https://github.com/home-operations/kopiur/commit/15e79ed2f35ab11d4b51b0e04cbb3916e34411b6))
- **tests:** have more clear errors and fix broken test ([73eea59](https://github.com/home-operations/kopiur/commit/73eea59e2d302ac6d71576d1cba7dffc4d762f04))

### Continuous Integration

- **github-action:** Update GitHub Artifact Actions (v7.0.0 → v8.0.1) ([#68](https://github.com/home-operations/kopiur/issues/68)) ([67d8cc4](https://github.com/home-operations/kopiur/commit/67d8cc4b117505921496be51df16c6eaa42c7b91))

## [0.2.0](https://github.com/home-operations/kopiur/compare/0.1.14...0.2.0) (2026-06-10)

### ⚠ BREAKING CHANGES

- **docs:** oops I don't know how to use semantic commits correctly

### Features

- **docs:** add to docs for helm chart ([99f9c5d](https://github.com/home-operations/kopiur/commit/99f9c5d02a93110a8c7fd2b869d4e5a596a67115))
- **docs:** oops I don't know how to use semantic commits correctly ([90f28d6](https://github.com/home-operations/kopiur/commit/90f28d6816ef5dc251084f4afb8476a0e75bad27))

### Documentation

- add break and feature adrs ([3f8c0b2](https://github.com/home-operations/kopiur/commit/3f8c0b2b1e1935fc29c1cf90453a8dfc5437e9d3))
- add more cel examples ([f5dc8ec](https://github.com/home-operations/kopiur/commit/f5dc8ec756a4cad6da3f501a43a22e53603adc8e))

## [0.1.14](https://github.com/home-operations/kopiur/compare/0.1.13...0.1.14) (2026-06-09)

### Features

- **docs:** add some more docs too ([7308b9e](https://github.com/home-operations/kopiur/commit/7308b9ee08efb6d892bc7b41288a1c3e1d32c2db))
- **docs:** also update docs for updated securityContext ([a778aad](https://github.com/home-operations/kopiur/commit/a778aadac6bb8c3c8ce08257abb4845b6cbbfad5))
- **security-context:** make sure to update securityContext docs ([8f6aa32](https://github.com/home-operations/kopiur/commit/8f6aa32033de7e9f662a40edd31961ef9358a581))
- **security:** implement fsGroup and more security context fun ([a352b57](https://github.com/home-operations/kopiur/commit/a352b57aa228d8ef0972a67ac7314a2d61eec521))
- **tests:** update e2e values to test projection ([3d3fdbf](https://github.com/home-operations/kopiur/commit/3d3fdbf81a878e518e65c920441de3b0a777ecc0))

## [0.1.13](https://github.com/home-operations/kopiur/compare/0.1.12...0.1.13) (2026-06-08)

### Features

- **docs:** add way more docs for restore ([533a54c](https://github.com/home-operations/kopiur/commit/533a54ceb4e1d3e368e3d8c168804bf726c651f3))
- **restore:** completely ship caching and Restore CRDs ([53a2bbe](https://github.com/home-operations/kopiur/commit/53a2bbea85243b2de9a154248dd5b9af507c0f81))

## [0.1.12](https://github.com/home-operations/kopiur/compare/0.1.11...0.1.12) (2026-06-08)

### Features

- **helm:** oops, secretProjection should be required opt-in ([9e2c849](https://github.com/home-operations/kopiur/commit/9e2c849bdbd9015cf1e5d8cb5b1552c3592293ce))

## [0.1.11](https://github.com/home-operations/kopiur/compare/0.1.10...0.1.11) (2026-06-08)

### Features

- **certs:** allow for self-signed certs instead of cert-manager as an option ([bb9cc5d](https://github.com/home-operations/kopiur/commit/bb9cc5d25e401b083bc95a9c04fbba5597b3d463))
- **certs:** implement tests and rbac for self-managed certs ([b91638a](https://github.com/home-operations/kopiur/commit/b91638a6b691d0a1d792baaee1efd40b5b0b34cc))
- **chart:** helm-docs README + values schema, release-time digest pinning ([#63](https://github.com/home-operations/kopiur/issues/63)) ([0cc4d3c](https://github.com/home-operations/kopiur/commit/0cc4d3c0aaf0602e3de6c6e07a2178f7cdaa7088))
- **dashboards:** support grafana operator dashboard thingy ([eb3b394](https://github.com/home-operations/kopiur/commit/eb3b394360198f3110c7b4e0f67cd4f761e80fd8))
- **docs:** add more useful docs ([93921db](https://github.com/home-operations/kopiur/commit/93921dbe158beb18072935f420487bc8855e6f73))
- **e2e:** preload the nfs image ([811fe3f](https://github.com/home-operations/kopiur/commit/811fe3f1565a359a3f3e92f65a36dfe25ed29ef6))
- **e2e:** update e2e for even more tests ([6a07512](https://github.com/home-operations/kopiur/commit/6a07512101fef353ac6e27f3e9135a71e7c618b1))
- **nfs:** I love e2e tests finding issues ([3488f52](https://github.com/home-operations/kopiur/commit/3488f52f8b66e16834254aaf5c98a18a50b6e8a4))
- **nfs:** support inline nfs to support onedr0p lol ([7c92884](https://github.com/home-operations/kopiur/commit/7c92884c4ef0a7bf200384623ff36703464c29e0))
- **secrets:** default-on secrets projection ([9896667](https://github.com/home-operations/kopiur/commit/9896667dd93268b851c6d5e6705943fceb5b9db7))
- **secrets:** implement secrets projection by default ([1921526](https://github.com/home-operations/kopiur/commit/1921526d1662cb76249559da7cb84ce2e1e9778c))
- **secrets:** jk secret projection is default opt-in ([cf69f61](https://github.com/home-operations/kopiur/commit/cf69f6113e2d16df19c7aa01d720a20ae8df1447))
- **secrets:** move projection into more granular CRDs ([e30f24f](https://github.com/home-operations/kopiur/commit/e30f24f5380a4d68d5394d817f521b5ec59fdbb3))
- **tests:** use a different nfs container for e2e testing ([e4b2816](https://github.com/home-operations/kopiur/commit/e4b28168ca8407617a9849982b17f8cd00491b26))

### Bug Fixes

- **prettier:** please stop messing with the CRDs oxfmt ([6c06aa9](https://github.com/home-operations/kopiur/commit/6c06aa9f2761674d9604c7d0f4973ee78a56066f))

## [0.1.10](https://github.com/home-operations/kopiur/compare/0.1.9...0.1.10) (2026-06-07)

### Features

- **backend:** also update support for various backends ([99d0942](https://github.com/home-operations/kopiur/commit/99d0942ec41cc97aaa5f46cf6a72073d251354f7))
- **docs:** add a slew of backend docs ([6bb1388](https://github.com/home-operations/kopiur/commit/6bb1388fb68672cb9daef014a5556d0c79691050))
- **docs:** create docs for the various backends ([49b19b3](https://github.com/home-operations/kopiur/commit/49b19b391e9236849935edd25b4073184a2a92d9))
- **docs:** migrate documentation site from mdBook to MkDocs Material ([527508c](https://github.com/home-operations/kopiur/commit/527508c9856d37a5353b35ae7e847c04c25f1e00))
- **docs:** promote rustdoc to a top-level header tab ([3c6d3c8](https://github.com/home-operations/kopiur/commit/3c6d3c809e4e4e2f7396b325301b1bdd763d9d89))
- **docs:** surface rustdoc in the MkDocs header ([ecc98a6](https://github.com/home-operations/kopiur/commit/ecc98a6dfb24695c57761a4e6d77362e43ee9344))
- **e2e:** make sure that values are consistent in e2e tests ([f5fe886](https://github.com/home-operations/kopiur/commit/f5fe8867e408ef119016757cc11a4174a14e9101))
- **tests:** implement more thorough e2e testing ([4e0d266](https://github.com/home-operations/kopiur/commit/4e0d26616d554a9a1b8f986d475299a2786b4e0b))
- **tests:** update broken unit test ([60d34be](https://github.com/home-operations/kopiur/commit/60d34beeb5364dd6e7a88c47ca0adc96ead00257))

## [0.1.9](https://github.com/home-operations/kopiur/compare/0.1.8...0.1.9) (2026-06-06)

### Features

- **dev:** update claude documentation skill ([43da6c8](https://github.com/home-operations/kopiur/commit/43da6c882e0ee9a03d153d1648224a8d46b944ad))
- **docs:** add more useful user-facing documentation ([b296ae0](https://github.com/home-operations/kopiur/commit/b296ae07a3e3c12ab33173ade98619a4bfd093a1))
- **e2e:** also add features to e2e tests ([8be3884](https://github.com/home-operations/kopiur/commit/8be388404d57e174dd27014b8d7de300b0d97cc0))
- **sa:** support stronger typing and testing for SA that goes cross namespace ([ff45c39](https://github.com/home-operations/kopiur/commit/ff45c3901cbe73f15c963be14b34bc3ccae03546))
- **tests:** continue to find wild issues through e2e testing, and resolve them ([77c3032](https://github.com/home-operations/kopiur/commit/77c30322fd3c9ad99964a1bf4184a4e42b7fa51c))

### Miscellaneous Chores

- we no longer have auto merge org wide ([23f3d39](https://github.com/home-operations/kopiur/commit/23f3d39768d2e9fe364db1af4369d30eb7c6a2d4))

## [0.1.8](https://github.com/home-operations/kopiur/compare/0.1.7...0.1.8) (2026-06-06)

### Features

- **docs:** add some useful docs ([6abd4f5](https://github.com/home-operations/kopiur/commit/6abd4f5421742d6f02228f7f269119d4ac5905f4))
- **docs:** implement docs for the movers ([b69f445](https://github.com/home-operations/kopiur/commit/b69f445173a88edc2017dc7cf6dfc65f433f0535))
- **docs:** take that mise ([ed2d9e2](https://github.com/home-operations/kopiur/commit/ed2d9e273ddb7f0f81c97285aa03a09140900a77))
- **maintenance:** make maintenance...actually do something ([e58232d](https://github.com/home-operations/kopiur/commit/e58232dacc1bcf056af82363fb767ab5aee6c4bc))
- **mover:** actually use secrets in movers and get rbac for it ([c8ab1de](https://github.com/home-operations/kopiur/commit/c8ab1deb9aa0a0b56ef1771d2a28571b99f9505d))
- **movers:** implement privileged movers ([334a9a5](https://github.com/home-operations/kopiur/commit/334a9a5919952e241883059f4ca375b813bac45a))
- **tests:** make sure to implement tests for updated rbac ([bb2a118](https://github.com/home-operations/kopiur/commit/bb2a118850eabc7b9d0ee9c3429ab897433a613c))

## [0.1.7](https://github.com/home-operations/kopiur/compare/0.1.6...0.1.7) (2026-06-06)

### Features

- **controller:** make sure not to spam the kube api every 0.33s ([9c4ca18](https://github.com/home-operations/kopiur/commit/9c4ca1817da9a34ade0530f77b67b0d768e3176f))
- **test:** create test to make sure we don't spam kube api ([2556258](https://github.com/home-operations/kopiur/commit/25562581ae099daba8b74d09c691ca6bfa71eab0))

### Miscellaneous Chores

- update rlspls config ([f9f4c70](https://github.com/home-operations/kopiur/commit/f9f4c707f1c43cd9deca761e058ebedfa3b931e1))

## [0.1.6](https://github.com/home-operations/kopiur/compare/0.1.5...0.1.6) (2026-06-06)

### Bug Fixes

- **deps:** update rust crate chrono (0.4.44 → 0.4.45) ([#46](https://github.com/home-operations/kopiur/issues/46)) ([30effc4](https://github.com/home-operations/kopiur/commit/30effc4a8eadab58fe26b4c41f9adbae6f9630a9))

## [0.1.5](https://github.com/home-operations/kopiur/compare/0.1.4...0.1.5) (2026-06-05)

### Features

- **errors:** provide more useful errors ([469947a](https://github.com/home-operations/kopiur/commit/469947aa00b460b145b050b3ef2cd15e74f2cf93))
- **tests:** also update tests so this writeable dir issue doesn't come back ([a6196f9](https://github.com/home-operations/kopiur/commit/a6196f912dce3cee82a9218f8dcdaa7ed4b7fa9c))

### Bug Fixes

- **controller:** make sure to mount writable paths for kopia ([f9fd3d5](https://github.com/home-operations/kopiur/commit/f9fd3d56d0543907ce07530fcf9ef2c4b1612ae1))

## [0.1.4](https://github.com/home-operations/kopiur/compare/0.1.3...0.1.4) (2026-06-05)

### Features

- **docs:** continue implementing rustdocs in crates ([b60b86a](https://github.com/home-operations/kopiur/commit/b60b86a7813510c9d73c8602b3499f480438c390))
- **docs:** make mdbook happy ([6d7f3d2](https://github.com/home-operations/kopiur/commit/6d7f3d2916470a2e241ca16633ff7a30eb9f88f6))
- **docs:** publish mdBook + rustdoc site to GitHub Pages ([74d7518](https://github.com/home-operations/kopiur/commit/74d7518a2dd6e6d382337c3abda6674dcbf3c85f))
- **docs:** serve docs site from kopiur.home-operations.com ([51d9ace](https://github.com/home-operations/kopiur/commit/51d9ace809125c42cb76aeecab4478a7cd0ac99a))
- **errors:** implement more error capturing for ease of use ([7fb10a1](https://github.com/home-operations/kopiur/commit/7fb10a1516662a0e9debe38b0d7e1267005de218))

### Bug Fixes

- **e2e:** resolve e2e errors for non-terminating pods ([cc83f67](https://github.com/home-operations/kopiur/commit/cc83f67b047c0bf851eccfb9e8d4475a5afeeafd))
- **mise:** try to resolve merge conflicts, again ([2b17028](https://github.com/home-operations/kopiur/commit/2b170282e1969dab4275b0608f34401bc41bdb22))

## [0.1.3](https://github.com/home-operations/kopiur/compare/0.1.2...0.1.3) (2026-06-04)

### Features

- **import:** allow Repository CRDs to be bootstrapped and imported ([b95d719](https://github.com/home-operations/kopiur/commit/b95d719b0741e4feb4d79de58dad1273d0cdb59f))
- **logs:** add some useful stdout logging to each container ([e44a9ca](https://github.com/home-operations/kopiur/commit/e44a9caf98aed5918bae0ef1a631a8b7ff93dfe3))
- **maintenance:** enable maintenance by default, but obviously allow overrides ([#48](https://github.com/home-operations/kopiur/issues/48)) ([c193929](https://github.com/home-operations/kopiur/commit/c193929a55f33bc06168febd51984a02921ebdba))

## [0.1.2](https://github.com/home-operations/kopiur/compare/0.1.1...0.1.2) (2026-06-04)

### Features

- **controller:** also add warning events if maintenace isn't configured ([5ba636b](https://github.com/home-operations/kopiur/commit/5ba636bcec78527bb267bc6aef1f293a6375ff5c))
- **rbac:** gonna need increased rbac perms for kubernetes event api push ([2f9f0a5](https://github.com/home-operations/kopiur/commit/2f9f0a5a47e992c270adf7d86221e583278a0735))

### Bug Fixes

- **mise:** pin rust to 1.95.0; correct renovate mise packageNames ([#36](https://github.com/home-operations/kopiur/issues/36)) ([5956f32](https://github.com/home-operations/kopiur/commit/5956f3249fe34ffbf35ce3d7eb98d4c69117b263))
- **schedule:** make sure to support `runOnCreate` ([0ebf046](https://github.com/home-operations/kopiur/commit/0ebf0463b3119599484b9e95dc4a2df03ac408d6))

### Miscellaneous Chores

- add 'cargo-llvm-cov' and 'cargo-deny' to package rules ([10db21b](https://github.com/home-operations/kopiur/commit/10db21b6c99be7c73b1014a29de1da1f37ffeedb))
- **mise:** lock file maintenance tool ([#38](https://github.com/home-operations/kopiur/issues/38)) ([31d117e](https://github.com/home-operations/kopiur/commit/31d117efb23b22b8efbd6931f9ddf03c703d7828))

## [0.1.1](https://github.com/home-operations/kopiur/compare/0.1.0...0.1.1) (2026-06-04)

### Features

- **charts:** bump up values for mem so e2e tests don't choke ([5fdace6](https://github.com/home-operations/kopiur/commit/5fdace6cf993de9a0a09a73494a3999f49974a20))
- **ci:** resolve broken release ci ([acd5fe7](https://github.com/home-operations/kopiur/commit/acd5fe7f8f32988ca787bfd3b065fff1db241402))
- **dev:** change CRD domain ([5eb5b28](https://github.com/home-operations/kopiur/commit/5eb5b28ea6684052dde721cae70866570058f2d5))
- **dev:** just a slight rename ([0a0d7b8](https://github.com/home-operations/kopiur/commit/0a0d7b8774f9cf9e46d412e2fdf4d0208eef1268))
- **dev:** update adr ([a0e37ed](https://github.com/home-operations/kopiur/commit/a0e37edb43d1c7d37cac73b36c067071ce98a69b))
- **dev:** update image prefix ([a41a676](https://github.com/home-operations/kopiur/commit/a41a676ab580ecd36035598cbb7fe4886f1cf468))
- **dev:** yay AGPL ([882445e](https://github.com/home-operations/kopiur/commit/882445e1d23f04a9755ca603ae448e036cbeed59))
- **everything:** also implement working e2e ([8a33b1c](https://github.com/home-operations/kopiur/commit/8a33b1c9fcbc50060e21601023188e2a9a1bbeb7))
- **everything:** implement the basics of the repo ([635ed2c](https://github.com/home-operations/kopiur/commit/635ed2cad321a9b4c9297a6145bc5b0394b982f5))
- **metrics:** also add docs for metrics addition ([1ac6f5e](https://github.com/home-operations/kopiur/commit/1ac6f5e24e71f6ddf642ed4a6109ebf9c9c28a8e))

### Bug Fixes

- **ci:** resolve issue with license in cargo-deny ([079002f](https://github.com/home-operations/kopiur/commit/079002fe51892e77d61bbcfe824b40008fdd8a40))
- **dev:** well I guess I got burnt on that merge conflict ([e2f3818](https://github.com/home-operations/kopiur/commit/e2f381871ceca323895230389b2fd6f9da280613))
- use the right trixie image ([c0f0ec5](https://github.com/home-operations/kopiur/commit/c0f0ec5abef853a04a0757aff8db0da6804f273f))

### Documentation

- **adr:** add kopia operator ADRs and kopiur Rust ADR ([caaa8a0](https://github.com/home-operations/kopiur/commit/caaa8a0ce647f8ca134bb5c69147dd201e0230bf))

### Miscellaneous Chores

- add mise and dotfiles ([#31](https://github.com/home-operations/kopiur/issues/31)) ([6ff63a8](https://github.com/home-operations/kopiur/commit/6ff63a8ce480627aa4ca6b4ccbff8eeeef1b2761))
- bring workflows up to the DAF ([#32](https://github.com/home-operations/kopiur/issues/32)) ([64666c3](https://github.com/home-operations/kopiur/commit/64666c3a8c53e9dcb496b73590c642f3145cfbb1))
- **deps:** lock file maintenance ([#30](https://github.com/home-operations/kopiur/issues/30)) ([940cc92](https://github.com/home-operations/kopiur/commit/940cc92a68188828783c1b50c8258143eb5506ef))
- update Dockerfiles to trixy ([f55e1ff](https://github.com/home-operations/kopiur/commit/f55e1ff89a1d1a7ed21fb8644a21e4aec7ad83fd))
