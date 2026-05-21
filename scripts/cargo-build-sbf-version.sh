# populate this on the stable branch
cargoBuildSbfVersion=4.0.0
cargoTestSbfVersion=4.0.0

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
