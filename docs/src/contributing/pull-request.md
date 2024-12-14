# The PR process for agave source code development

## Preparation

1. You’ll want a powerful machine for this. Building agave from sources takes a lot of RAM, make sure you have ~30GB to be sure it is enough.
   1. Rust analyzer is quite capable of running on agave source tree, but expect high memory consumption
2. Make a fork of `anza-xyz/agave` master branch under your own namespace in github. Let us assume it is called `ubercoder/agave`
   1. Do not ever commit anything into the master branch of your fork. Keep it synced to upstream, this will make rebase process painless.
   2. Whenever you want to pull agave code, pull it from your fork not from agave repo directly.
3. Follow a guide to set up a reasonable rust development environment, e.g. [this one](https://github.com/anza-xyz/agave/?tab=readme-ov-file#building)
   1. Make sure you can at least build the code before proceeding
4. Plan the minimal viable set of changes you'd need to achieve your goals. Idea is to keep your PRs as small and simple as feasible.


## When not to file a PR

1. Your code/comments may expose/highlight a security threat in the existing deployed instances


## Procedure

1. Make a feature branch on ubercoder/agave repo, e.g. `fix_all_bugs`
2. Pull from `ubercoder/agave` the master and `fix_all_bugs` branches
3. Make any necessary changes to `fix_all_bugs`
   1. Commit and push as appropriate, make sure your commits are sensibly sized
4. To keep track of changes in upstream `anza-xyz/agave`:
   1. Sync the `ubercoder/agave` master branch with upstream. You can do this with cli or github. **This should always fast-forward** since you are never committing anything to  ubercoder/agave master.
   2. Pull the latest version of ubercoder/agave master to your local machine if you have updated your fork via github
   3. Checkout the `fix_all_bugs` branch and rebase it onto master's head (`git rebase master`) to pull in the changes made in upstream’s master into your feature branch. Fix conflicts if necessary. Follow applicable rebase guides.
   4. Now you can force-push the `fix_all_bugs ` branch to rewrite its history on the gitlabserver (necessary due to rebase that was made)
5. Make sure you run this procedure before every commit that you intend to push for review:
   1. Make sure your code builds without warnings - `./cargo build` is a good start
   2. Run tests on all packages you have worked on ``` ./cargo test \--package \<crate\_name\> ```
      1. Just running cargo test in the root directory will run a bunch of tests that will fail on your machine, only run the tests on the crates you touch.
   3. If your crate has examples, they should get built by cargo test also, but they will not get run. Make sure they work correctly, CI will not hold your hand here.
   4. ``` /scripts/cargo-fmt.sh && ./scripts/cargo-clippy.sh```
   5. Make sure your Cargo.toml files are *perfect* [by sorting them](#unsorted-deps)
   6. Make sure there are no trailing whitespaces anywhere `git diff origin/master --check --oneline`
      1. If you find any in files that cargo fmt will not fix for you, you can use e.g. `sed  's/[ \t]*$//'` to fix them manually
   7. You can manually run an approximation of full github CI with ``` ./ci/run-local.sh``` to diagnose any problems
   8. Once all of the above checks pass, make the final commit and push `fix_all_bugs` branch to github
      1. Make sure you [squash unnecessary commits](#pr-squash-commits) to ensure your commit log is sensible
6. To create a PR:
   1. Go to `ubercoder/agave` on github, switch to `fix_all_bugs` branch, and make PR against `anza-xyz/agave` master
   2. Make sure you explain exactly why this PR is useful on its own merit
   3. If you expect to follow with more PRs to expand on a feature, make it clear
7. If you are not agave core developer, your PR will be reviewed by one, and, assuming it gets approved, CI process will be enabled for it.
   1. If you are a developer, consider choosing “make draft PR” option so your PR does not pollute the PR list before you are sure that CI checks pass
8. Wait for github to run CI, this will take about 1 hour. Make sure CI passes, and your PR will be ready for final review
   1. Find some 2-3 nice reviewers to go over your code. If you do not know whom to choose as reviewer, ask around on discord.
   2. Remeber, reviewers are humans and have feelings too.
   3. If CI does not pass, fix whatever is necessary and push again into the `fix_all_bugs` branch
10. Gotchas:
   1. If any of the Cargo.lock files are changed by the steps above, make sure to commit them too, else CI will punish you with inscrutable errors
   2. In some cases it may be necessary to manually [coerce Cargo.lock updates](#coerce-cargo.lock-updates)


# Tips and Tricks

## Unsorted deps {#unsorted-deps}

  If your Cargo.toml is not perfectly following dtonlay's recommendations, CI will not approve your commit. Use cargo-sort to fix that.
```sh
./cargo install cargo-sort
./cargo sort
```


## Convince a build script that your crate has correct version

   In your Cargo.toml you will normally want to inherit the workspace version, which cargo new will default-init to

```toml
version.workspace = true
```

   But it will not work, you need


```toml
version = { workspace = true }
```

   And yes, they do the same exact thing. But the first one will fail in CI. No CI is perfect.



## Coerce Cargo.lock updates {#coerce-cargo.lock-updates}

   Sometimes Cargo.lock files can be feisty, to manually coerce Cargo.lock updates, use


```sh
./scripts/cargo-for-all-lock-files.sh tree
./scripts/cargo-for-all-lock-files.sh check --locked --tests --bins
```



## Squash the unnecessary commits{#pr-squash-commits}

1. You may want to set your EDITOR variable to the editor of choice prior to starting this.
2. `git rebase -i <commit-right-before-your-first-commit>`
3. You'll then see all of your commits from oldest to newest like this:
```
pick 109c178 Improve TPS by 20 percent
pick 0737bd6 Remove useless mutex in turbine
...
pick <commit hash X> <commit message X>
pick <commit hash Y> forgot a file
pick <commit hash Z> pleasing CI
...
pick 3f96c5c better naming for new structures
```
4. To squash the trivial commits Y and Z into X (maybe they are all related or commits Y and Z are just formatting/clippy), you would update the file to look like:
```
pick 109c178 Improve TPS by 20 percent
pick 0737bd6 Remove useless mutex in turbine
...
pick <commit hash X> <commit message X>
s <commit hash Y> forgot a file
s <commit hash Z> pleasing CI
...
pick 3f96c5c better naming for new structures
```
^ the `s` means you want to "squash" the commit into the commit above. Do not touch the commit message text yet, it will not do anything.
Then save the file and quit the editor to schedule squashes to git.

Next git will pop another editor window to form the commit message for the new commit X (which will now include squashed Y and Z)

* Typically, if you need to keep Y and Z commit messages in the new commit message for X, then they shouldn't be squashed.
You can thus safely comment out the commit messages Y and Z so it just will show commit message X in the final version.
* If you have squashed into several commits, you’ll have to edit several files with commit messages. Keep on modifying the commit messages as appropriate.
* Finally, the rebase will be executed. Now if you do `git log`, you will see that commits Y and Z no longer exist but are now squashed into commit X.
* It is a good idea to double check that the code is what you expect before you commit
