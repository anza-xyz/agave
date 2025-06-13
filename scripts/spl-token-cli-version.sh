# populate this on the stable branch
splTokenCliVersion=5.3.0

maybeSplTokenCliVersionArg=
if [[ -n "$splTokenCliVersion" ]]; then
    # shellcheck disable=SC2034
    maybeSplTokenCliVersionArg="--version $splTokenCliVersion"
fi
