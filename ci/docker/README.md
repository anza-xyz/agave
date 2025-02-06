Docker image containing rust, rust nightly and some preinstalled packages used in CI

This image is manually maintained:

#### CLI

1. Edit
   1. `ci/rust-version.sh` for rust and rust nightly version
   2. `ci/docker/env.sh` for some environment variables
   3. `ci/docker/Dockerfile` for some other packages
2. Ensure you're a member of the [Solana Docker Hub Organization](https://hub.docker.com/u/solanalabs/) and already `docker login`
3. Run `ci/docker/build.sh` to build/publish the new image
