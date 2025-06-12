#!/usr/bin/env python3

# This script:
# - searches given manifest file for solana / agave dependencies
# - modifies each found line in the file by adding a custom registry parameter
# - writes the output back to the given manifest file
#

import json
import os
import subprocess
import sys
import tomllib

if len(sys.argv) != 3:
    print('Usage: %s <manifest path> <crate-name>' % sys.argv[0])

real_file = os.path.realpath(__file__)
ci_path = os.path.dirname(real_file)
src_root = os.path.dirname(ci_path)
cargo_toml = os.path.join(src_root, sys.argv[1])
pkg_name = sys.argv[2]
version = os.environ.get('CI_TAG')


def load_metadata():
    cmd = f'{src_root}/cargo metadata --no-deps --format-version=1'
    return json.loads(subprocess.Popen(
        cmd, shell=True, stdout=subprocess.PIPE).communicate()[0])


def get_pkg_deps(package_name):
    metadata = load_metadata()
    dependency_graph = dict()

    for pkg in metadata['packages']:
        dependency_graph[pkg['name']] = [
            x['name'] for x in pkg['dependencies'] if x['name'].startswith(('solana', 'agave')) and x['source'] is None
        ]
    return dependency_graph[package_name]


with open(cargo_toml, 'rb', 0) as file:
    data = file.readlines()
    deps = get_pkg_deps(pkg_name)

    for idx, line in enumerate(data):
        split_line = line.decode().split(" =", 1)
        if split_line[0] in deps and not split_line[1].startswith(' { registry'):
            tmp = split_line[1].replace("workspace = true", "version = \"" + version + "\"")[3:]
            data[idx] = bytes(split_line[0] + ' = { registry = "kellnr", ' + tmp, 'utf-8')

with open(cargo_toml, 'wb') as file:
    file.writelines(data)
