# populate this on the stable branch
splTokenCliVersion=5.5.0

maybeSplTokenCliVersionArg=
if [[ -n "$splTokenCliVersion" ]]; then
    # shellcheck disable=SC2034
    maybeSplTokenCliVersionArg="--version $splTokenCliVersion"
fi
