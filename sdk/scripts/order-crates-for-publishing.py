#!/usr/bin/env python3
#
# This script figures the order in which workspace crates must be published to
# crates.io.  Along the way it also ensures there are no circular dependencies
# that would cause a |cargo publish| to fail.
#
# On success an ordered list of Cargo.toml files is written to stdout
#

import os
import json
import subprocess
import sys;

real_file = os.path.realpath(__file__)
scripts_path = os.path.dirname(real_file)
src_root = os.path.dirname(scripts_path)

def load_metadata():
    cmd = f'cargo metadata --no-deps --format-version=1'
    return json.loads(subprocess.Popen(
        cmd, shell=True, stdout=subprocess.PIPE).communicate()[0])

# Consider a situation where a crate now wants to use already existing
# developing-oriented library code for their integration tests and benchmarks,
# like creating malformed data or omitting signature verifications. Ideally,
# the code should have been guarded under the special feature
# `dev-context-only-utils` to avoid accidental misuse for production code path.
#
# In this case, the feature needs to be defined then activated for the crate
# itself. To that end, the crate actually needs to depend on itself as a
# dev-dependency with `dev-context-only-utils` activated, so that the feature
# is conditionally activated only for integration tests and benchmarks. In this
# way, other crates won't see the feature activated even if they normal-depend
# on the crate.
#
# This self-referencing dev-dependency can be thought of a variant of
# dev-dependency cycles and it's well supported by cargo. The only exception is
# when publishing. In general, cyclic dev-dependency doesn't work nicely with
# publishing: https://github.com/rust-lang/cargo/issues/4242 .
#
# However, there's a work around supported by cargo. Namely, it will ignore and
# strip these cyclic dev-dependencies when publishing, if explicit version
# isn't specified: https://github.com/rust-lang/cargo/pull/7333 (Released in
# rust 1.40.0: https://releases.rs/docs/1.40.0/#cargo )
#
# This script follows the same safe discarding logic to exclude these
# special-cased dev dependencies from its `dependency_graph` and further
# processing.
def is_self_dev_dep_with_dev_context_only_utils(package, dependency, wrong_self_dev_dependencies):
    no_explicit_version = '*'

    is_special_cased = False
    if (dependency['kind'] == 'dev' and
        dependency['name'] == package['name'] and
        'dev-context-only-utils' in dependency['features'] and
        'path' in dependency):
        is_special_cased = True
        if dependency['req'] != no_explicit_version:
            # it's likely `{ workspace = true, ... }` is used, which implicitly pulls the
            # version in...
            wrong_self_dev_dependencies.append(dependency)

    return is_special_cased


# `cargo publish` is fine with circular dev-dependencies if
# they are path deps.
# However, cargo still fails if deps are path deps with versions
# (this when you use `workspace = true`): https://github.com/rust-lang/cargo/issues/4242
# Unlike in is_self_dev_dep_with_dev_context_only_utils(),
# we don't have a clean way of checking if someone used a workspace dev
# dep when they probably meant to use a path dev dep,
# so this function just checks if a dev dep is a path dep
# and provides no special warnings.
def is_path_dev_dep(dependency):
    no_explicit_version = '*'
    return (
        dependency['kind'] == 'dev'
        and 'path' in dependency
        and dependency['req'] == no_explicit_version
    )

def should_add(package, dependency, wrong_self_dev_dependencies):
    related_to_solana = dependency['name'].startswith('solana')
    self_dev_dep_with_dev_context_only_utils = is_self_dev_dep_with_dev_context_only_utils(
        package, dependency, wrong_self_dev_dependencies
    )
    return (
        related_to_solana
        and not self_dev_dep_with_dev_context_only_utils
        and not is_path_dev_dep(dependency)
    )

def get_packages():
    metadata = load_metadata()

    manifest_path = dict()

    # Build dictionary of packages and their immediate solana-only dependencies
    dependency_graph = dict()
    wrong_self_dev_dependencies = list()

    for pkg in metadata['packages']:
        manifest_path[pkg['name']] = pkg['manifest_path'];
        dependency_graph[pkg['name']] = [
            x['name'] for x in pkg['dependencies'] if should_add(pkg, x, wrong_self_dev_dependencies)
        ];

    # Check for direct circular dependencies
    circular_dependencies = set()
    for package, dependencies in dependency_graph.items():
        for dependency in dependencies:
            if dependency in dependency_graph and package in dependency_graph[dependency]:
                circular_dependencies.add(' <--> '.join(sorted([package, dependency])))

    for dependency in circular_dependencies:
        sys.stderr.write('Error: Circular dependency: {}\n'.format(dependency))
    for dependency in wrong_self_dev_dependencies:
        sys.stderr.write('Error: wrong dev-context-only-utils circular dependency. try: ' +
            '{} = {{ path = ".", features = {} }}\n'
            .format(dependency['name'], json.dumps(dependency['features']))
        )

    if len(circular_dependencies) != 0 or len(wrong_self_dev_dependencies) != 0:
        sys.exit(1)

    # Order dependencies
    sorted_dependency_graph = []
    max_iterations = pow(len(dependency_graph),2)
    while dependency_graph:
        deleted_packages = []
        if max_iterations == 0:
            # One day be more helpful and find the actual cycle for the user...
            sys.exit('Error: Circular dependency suspected between these packages: \n {}\n'.format('\n '.join(dependency_graph.keys())))

        max_iterations -= 1

        for package, dependencies in dependency_graph.items():
            if package in deleted_packages:
                continue
            for dependency in dependencies:
                if dependency in dependency_graph:
                    break
            else:
                deleted_packages.append(package)
                sorted_dependency_graph.append((package, manifest_path[package]))

        dependency_graph = {p: d for p, d in dependency_graph.items() if not p in deleted_packages }


    return sorted_dependency_graph

for package, manifest in get_packages():
    print(os.path.relpath(manifest))
