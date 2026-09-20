# populate this on the stable branch
cargoBuildSbfVersion=

maybeCargoBuildSbfVersionArg=
if [[ -n "$cargoBuildSbfVersion" ]]; then
    # shellcheck disable=SC2034
    maybeCargoBuildSbfVersionArg="--version $cargoBuildSbfVersion"
fi
