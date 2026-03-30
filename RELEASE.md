# Agave Release process

## Branches and Tags

```
========================= master branch (edge channel) =======================>
         \                      \                     \
          \___v0.7.0 tag         \                     \
           \                      \         v0.9.0 tag__\
            \          v0.8.0 tag__\                     \
 v0.7.1 tag__\                      \                 v0.9 branch (beta channel)
              \___v0.7.2 tag         \___v0.8.1 tag
               \                      \
                \                      \
           v0.7 branch         v0.8 branch (stable channel)

```

### master branch
All new development occurs on the `master` branch.

Bug fixes that affect a `vX.Y` branch are first made on `master`.  This is to
allow a fix some soak time on `master` before it is applied to one or more
stabilization branches.

Merging to `master` first also helps ensure that fixes applied to one release
are present for future releases.  (Sometimes the joy of landing a critical
release blocker in a branch causes you to forget to propagate back to
`master`!)"

Once the bug fix lands on `master` it is cherry-picked into the `vX.Y` branch
and potentially the `vX.Y-1` branch.  The exception to this rule is when a bug
fix for `vX.Y` doesn't apply to `master` or `vX.Y-1`.

Immediately after a new stabilization branch is forged, the `Cargo.toml` minor
version (*Y*) in the `master` branch is incremented by the release engineer.
Incrementing the major version of the `master` branch is outside the scope of
this document.

### v*X.Y* stabilization branches
These are stabilization branches. They are created from the `master` branch approximately
every 13 weeks.

### v*X.Y.Z* release tag
The release tags are created as desired by the owner of the given stabilization
branch, and cause that *X.Y.Z* release to be shipped to https://crates.io

Immediately after a new v*X.Y.Z* branch tag has been created, the `Cargo.toml`
patch version number (*Z*) of the stabilization branch is incremented by the
release engineer.

## Channels
Channels are used by end-users (humans and bots) to consume the branches
described in the previous section, so they may automatically update to the most
recent version matching their desired stability.

There are three release channels that map to branches as follows:
* edge - tracks the `master` branch, least stable.
* beta - tracks the largest (and latest) `vX.Y` stabilization branch, more stable.
* stable - tracks the second largest `vX.Y` stabilization branch, most stable.

## Steps to Create a Branch

### Major release branch
1. If the new branch will be the first branch of a new major release check that
all eligible deprecated symbols have been removed. Our policy is to deprecate
for at least one full minor version before removal.

### Create the new branch
1. Check out the latest commit on `master` branch:
    ```
    git fetch --all
    git checkout upstream/master
    ```
1. Determine the new branch name.  The name should be "v" + the first 2 version fields
   from Cargo.toml.  For example, a Cargo.toml with version = "0.9.0" implies
   the next branch name is "v0.9".
1. Create the new branch and push this branch to the `agave` repository:
    ```
    git checkout -b <branchname>
    git push -u origin <branchname>
    ```

Alternatively use the Github UI.

### Update master branch to the next release minor version

1. After the new branch has been created and pushed, update the Cargo.toml files on **master** to the next semantic version (e.g. 0.9.0 -> 0.10.0) with:
     ```
     $ scripts/increment-cargo-version.sh minor
     ```
1. Push all the changed Cargo.toml and Cargo.lock files to the `master` branch with something like:
    ```
    git co -b version_update
    git ls-files -m | xargs git add
    git commit -m 'Bump version to X.Y+1.0'
    git push -u origin version_update
    ```
1. Confirm that your freshly cut release branch is shown as `BETA_CHANNEL` and the previous release branch as `STABLE_CHANNEL`:
    ```
    ci/channel-info.sh
    ```

### Update the Changelog

Create a PR that makes the following updates to [CHANGELOG.md](https://github.com/anza-xyz/agave/blob/master/CHANGELOG.md) in master:
* Advance the channel links with the newly created branch becoming beta.
* Add a new section `X.Y.0-Unreleased` for the new master version.
* Remove the `Unreleased` annotation for the section that has now become beta.

### Miscellaneous Clean up

#### Newly Promoted Stable (Former Beta) Branch

1. Pin the spl-token-cli version in the newly promoted stable branch by setting `splTokenCliVersion` in scripts/spl-token-cli-version.sh to the latest release that depends on the stable branch (usually this will be the latest spl-token-cli release).
1. Pin the cargo-build-sbf and cargo-test-sbf versions in the newly promoted stable branch by setting `cargoBuildSbfVersion` and `cargoTestSbfVersion` in scripts/cargo-build-sbf-version.sh to the latest release that depends on the stable branch (usually this will be the latest releases).

#### Newly Created Beta Branch

1. Update [CHANGELOG.md](https://github.com/anza-xyz/agave/blob/master/CHANGELOG.md) to remove the channel links on the new branch. Additionally, remove any wording about the new branch being unreleased.
1. Update [CODEOWNERS](https://github.com/anza-xyz/agave/blob/master/.github/CODEOWNERS) to `* @anza-xyz/backport-reviewers` on the new branch.
1. Update [mergify.yml](https://github.com/anza-xyz/agave/blob/master/.mergify.yml) to add backport actions for the new branch and remove actions for the obsolete branch.
1. Adjust the [Github backport labels](https://github.com/anza-xyz/agave/labels) to add the new branch label and remove the label for the obsolete branch.
1. Announce on Discord #development that the release branch exists so people know to use the new backport labels.

## Steps to Create a Release

### Create the Release Tag on GitHub

1. Check out relevant branch, create release tag and push it. The release tag must exactly match the `version` field in `/Cargo.toml` prefixed by `v`.
    ```
    git checkout v4.0
    git tag v4.0.1
    git push upstream v4.0.1
    ```
1. [The automation](https://github.com/anza-xyz/agave/blob/master/.github/workflows/release.yml) will create the new draft release, start `agave-secondary` Buildkite pipeline and create a PR to bump the version in the branch.
1. Go to [GitHub Releases](https://github.com/anza-xyz/agave/releases) and edit the draft release just made by the automation.
1. Fill the release notes.
   1.  If this is the first release on the branch (e.g. v0.13.**0**), paste in [this
   template](https://raw.githubusercontent.com/anza-xyz/agave/master/.github/RELEASE_TEMPLATE.md).  Engineering Lead can provide summary contents for release notes if needed.
   1. If this is a patch release, review all the commits since the previous release on this branch and add details as needed.
1. Ensure the release is marked **"This is a pre-release"**.  This flag will need to be removed manually after confirming the Linux binary artifacts appear at a later step.

### Verify release automation success
Go to [Agave Releases](https://github.com/anza-xyz/agave/releases) and click on the latest release that you just published.
Verify that all of the build artifacts are present (15 assets), then uncheck **"This is a pre-release"** for the release.

Build artifacts can take up to 60 minutes after creating the tag before
appearing.  To check for progress:
* The `agave-secondary` Buildkite pipeline handles creating the Linux and macOS release artifacts and updated crates.  Look for a job under the tag name of the release: https://buildkite.com/anza-xyz/agave-secondary.
* The Windows release artifacts are produced by GitHub Actions.  Look for a job under the tag name of the release: https://github.com/anza-xyz/agave/actions.

[Crates.io agave-validator](https://crates.io/crates/agave-validator) should have an updated agave-validator version.  This can take 2-3 hours, and sometimes fails in the `agave-secondary` job.
If this happens and the error is non-fatal, click "Retry" on the "publish crate" job
