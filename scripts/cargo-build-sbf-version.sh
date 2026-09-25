# populate this on the stable branch
cargoBuildSbfVersion=4.4.0

maybeCargoBuildSbfVersionArg=
if [[ -n "$cargoBuildSbfVersion" ]]; then
    # shellcheck disable=SC2034
    maybeCargoBuildSbfVersionArg="--version $cargoBuildSbfVersion"
fi
