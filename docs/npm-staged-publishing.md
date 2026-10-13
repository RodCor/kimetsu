# npm staged publishing and release recovery

The release workflow stages npm packages for human approval. A successful staging
job does **not** mean the packages are publicly available. GitHub release assets
and crates.io publication remain separate.

We use Node 24 and npm 11.21.0. npm documents Node 22.14+ and npm 11.15+ as the
minimum for staged publishing. The existing `NPM_TOKEN` can stay stage-only;
this workflow does not require direct-publish permission, change account or
trusted-publisher settings, or approve/reject stages. `PUBLISH_NPM=true` remains
the repository gate. Lifecycle scripts are disabled while packing/staging. The
SDK is built separately without the npm credential and retains its own version.

## Recover an existing release

After this workflow is merged and its checks pass, dispatch **stage npm release**
from `main`, setting `release_tag` to the existing stable tag (for example
`v2.9.0`). Equivalently:

```sh
gh workflow run npm-stage.yml --ref main -f release_tag=v2.9.0
```

This uses package sources from the tag and downloads the existing release assets.
It checks that the tag's commit belongs to main, that Cargo's version agrees,
and that the release is public and stable. It verifies the checksum manifest's
GitHub attestation against that exact tag/commit and the original release workflow,
then verifies the asset checksums. It never rebuilds binaries, changes tags or
release assets, or reruns crates.io publication. npm provenance describes the
staging workflow run; the verified original asset attestation binds binaries to
the release commit.

The job summary reports each package as already public, newly staged, or already
staged, including stage IDs where applicable. Public versions are skipped.
An existing stage is reused only if its package, version, tarball SHA-1 and
`latest` tag match. Conflicts and registry/authentication errors stop the job;
they are not treated as permission to resubmit, reject, or bypass approval.
If a submission's result is uncertain, inspect the stage list before retrying.
Partial stages remain private until approved.

## Maintainer approval

Review the stage IDs in the job summary in npm's **Staged Packages** interface,
or inspect them using npm 11.21.0:

```sh
npm stage list @kimetsu-ai/linux-x64 --json
npm stage view <stage-id>
npm stage download <stage-id>
npm stage approve <stage-id>
```

Approval is an authenticated maintainer action with interactive 2FA. Do not put
an OTP or credentials in a workflow, issue, or chat. The workflow never invokes
`approve`. Review downloaded tarballs, versions, metadata, provenance and binaries
before approval; a matching SHA-1 only establishes retry identity.

Approve the four CLI platform packages and three remote platform packages first.
Verify each exact version is public using `npm view <package>@<version> version`
before approving the `kimetsu-ai` and `kimetsu-remote` wrappers that depend on them.
The SDK is independent. Approval is per package, not an atomic multi-package
transaction; a failed or partial approval must be resolved before approving a
dependent wrapper. Finally verify all public versions and test installation.

The helper tests simulate registry and npm responses. They do not demonstrate
real-service staging, provenance acceptance, or human approval; those require a
successful authorized workflow run and maintainer approval.

Official references:
- [Staged publishing](https://docs.npmjs.com/staged-publishing/)
- [npm stage commands](https://docs.npmjs.com/cli/v11/commands/npm-stage/)
- [Trusted publishing](https://docs.npmjs.com/trusted-publishers/) (a separate setup,
  not enabled by this change)
