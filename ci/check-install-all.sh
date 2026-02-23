source scripts/spl-token-cli-version.sh
if [[ -z $splTokenCliVersion ]]; then
    echo "On the stable channel, splTokenCliVersion must be set in scripts/spl-token-cli-version.sh"
    exit 1
fi

source scripts/cargo-build-sbf-version.sh
if [[ -z $cargoBuildSbfVersion ]]; then
    echo "On the stable channel, cargoBuildSbfVersion must be set in scripts/cargo-build-sbf-version.sh"
    exit 1
fi
if [[ -z $cargoTestSbfVersion ]]; then
    echo "On the stable channel, cargoTestSbfVersion must be set in scripts/cargo-build-sbf-version.sh"
    exit 1
fi
