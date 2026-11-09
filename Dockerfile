FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl fish git llvm make opensbi python3 qemu-system-misc sudo \
    build-essential && rm -rf /var/lib/apt/lists/*
RUN useradd -m -s /bin/bash vscode && echo 'vscode ALL=(ALL) NOPASSWD:ALL' >> /etc/sudoers
USER vscode
ENV PATH=/home/vscode/.cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
    --default-toolchain nightly-2025-01-18 && \
    rustup target add riscv64imac-unknown-none-elf && \
    rustup component add rustfmt clippy rust-analyzer
WORKDIR /workspace
CMD ["fish"]
