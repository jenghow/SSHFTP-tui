# Minimal OpenSSH server for the end-to-end tests.
#
# Debian/Ubuntu based. The default base is the .NET runtime image because it is
# Debian bookworm and is guaranteed to be present in this build environment;
# any slim Debian/Ubuntu image works:
#
#   podman build --build-arg BASE_IMAGE=debian:bookworm-slim -t sshftp-tui-test tests/sshd
ARG BASE_IMAGE=mcr.microsoft.com/dotnet/aspnet:9.0
FROM ${BASE_IMAGE}

RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-server passwd \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /etc/ssh/keys /run/sshd

# Unprivileged account used by the integration test (password auth only).
RUN useradd --create-home --shell /bin/bash tester \
    && echo 'tester:testpass' | chpasswd

COPY sshd_config /etc/ssh/sshd_config
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh && sed -i 's/\r$//' /entrypoint.sh

EXPOSE 2222
ENTRYPOINT ["/entrypoint.sh"]
