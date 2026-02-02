ARG BASE_IMAGE=
FROM ${BASE_IMAGE}

RUN apt-get update && \
    apt-get install -y bzip2 libssl-dev ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY usr/bin /usr/bin/

ENTRYPOINT [ "/usr/bin/solana" ]
