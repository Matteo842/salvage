#!/bin/sh
# Builds the Linux release tarball. Run from the repo root on Linux (or WSL):
#   packaging/linux/build.sh
# Needs rustup, zig and cargo-zigbuild. Linking against glibc 2.31 keeps the
# binary running on older distros too (Ubuntu 20.04, Debian 11 and newer).
set -eu

target=x86_64-unknown-linux-gnu
glibc=2.31
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
name="salvage-$version-linux-x86_64"
out="target/dist/$name"

rustup target add "$target" >/dev/null
cargo zigbuild --release --target "$target.$glibc"

rm -rf "$out"
mkdir -p "$out"
cp "target/$target/release/salvage" "$out/"
"$out/salvage" --write-icon "$out/salvage.png"
cp packaging/linux/salvage.desktop packaging/linux/install.sh README.md LICENSE "$out/"
chmod +x "$out/install.sh" "$out/salvage"
tar -C target/dist -czf "target/dist/$name.tar.gz" "$name"
echo "target/dist/$name.tar.gz"
