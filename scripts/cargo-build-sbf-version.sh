# populate this on the stable branch
cargoBuildSbfVersion=
cargoTestSbfVersion=

maybeCargoBuildSbfVersionArg=
if [[ -n "$cargoBuildSbfVersion" ]]; then
    # shellcheck disable=SC2034
    maybeCargoBuildSbfVersionArg="--version $cargoBuildSbfVersion"
fi

maybeCargoTestSbfVersionArg=
if [[ -n "$cargoTestSbfVersion" ]]; then
    # shellcheck disable=SC2034
    maybeCargoTestSbfVersionArg="--version $cargoTestSbfVersion"
fi
