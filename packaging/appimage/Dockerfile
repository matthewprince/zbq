# ZBQ's Linux release build image. Ubuntu 22.04 (glibc 2.35), so the AppImage
# runs on Ubuntu 22.04, Debian 12 and anything newer. Qt 6.9 with Qt WebEngine
# comes from the host (mounted at /qt by build.sh); everything else is here.
FROM ubuntu:22.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential pkg-config cmake clang libclang-dev nasm \
      libasound2-dev libjack-jackd2-dev libdbus-1-dev libssl-dev \
      libgl1-mesa-dev libegl1-mesa-dev libxkbcommon-dev libwayland-dev \
      libxkbcommon-x11-0 libxcb-cursor0 libxcb-icccm4 libxcb-image0 \
      libxcb-keysyms1 libxcb-render-util0 libxcb-xinerama0 libxcb-xkb1 \
      libxcb-shape0 libxcb-randr0 libxcb-sync1 libxcb-xfixes0 libxcb-glx0 \
      libwayland-client0 libwayland-egl1 libwayland-cursor0 libegl1 libgl1 \
      libnss3 libnspr4 libxcomposite1 libxdamage1 libxrandr2 libxtst6 libxkbfile1 \
      libxshmfence1 libgbm1 libdrm2 libxss1 libx11-xcb1 libfontconfig1 \
      libharfbuzz0b libglib2.0-0 libdbus-1-3 libpulse0 libcups2 libxi6 libgtk-3-0 \
      patchelf file binutils dbus-daemon python3 curl ca-certificates xz-utils \
      desktop-file-utils \
 && rm -rf /var/lib/apt/lists/*
ARG RUST_VERSION=1.97.1
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal \
      --default-toolchain "$RUST_VERSION" --no-modify-path \
 && chmod -R a+rwX /opt/cargo /opt/rustup
