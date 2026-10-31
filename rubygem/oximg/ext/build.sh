#!/bin/sh
# Build the probe library (liboximg_probe) for the platform this runs
# on, and print its path. The one recipe `rake compile`, CI and the
# release all use, so a flag fixed here is fixed for every build.
#
#   rubygem/oximg/ext/build.sh [target-dir]   (default: <repo>/target/oximg-probe)
#
# It starts from a copy of the root Cargo.lock, which cargo prunes to
# what the library uses, so the library builds the decoders the
# executable ships. ext/ keeps no lock of its own: a committed one
# would drift from the root lock, and would refuse --locked builds on
# every version bump, since it pins oximg itself.
#
# The library must load wherever the platform gem installs, needing
# only libc and libgcc_s (Rust's unwinder; Ruby needs it too, so it is
# there wherever Ruby is). jpegli's build script links libstdc++ by name
# although nothing the probe keeps comes from it, and a linker that
# does not drop unused libraries (Debian's, Alpine's) leaves the
# library needing libstdc++.so.6, which a bare Alpine or slim image
# lacks. So C++ runtime is linked statically, and the build fails if
# the result still names libstdc++.
set -eu

ext=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$ext/../../.." && pwd)
target_dir=${1:-$root/target/oximg-probe}

case "$(uname -s)" in
  Linux)
    flags="-C link-arg=-static-libgcc"
    if ldd --version 2>&1 | grep -qi musl; then
      # musl targets link statically by default, and rustc then drops
      # a cdylib with only a warning and a zero exit. A shared library
      # needs the dynamic C runtime. -static-libstdc++ does not cover
      # the explicit -lstdc++ jpegli asks for; the wrapper turns that
      # into the static archive.
      flags="$flags -C target-feature=-crt-static -C linker=$ext/link-static-libstdcxx.sh"
    else
      flags="$flags -C link-arg=-static-libstdc++"
    fi
    export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$flags"
    library=liboximg_probe.so
    ;;
  Darwin)
    # The system libc++ is part of macOS, as for every macOS binary.
    library=liboximg_probe.dylib
    ;;
  *)
    echo "build.sh: no probe library recipe for $(uname -s)" >&2
    exit 1
    ;;
esac

cp "$root/Cargo.lock" "$ext/Cargo.lock"
cargo build --release --manifest-path "$ext/Cargo.toml" --target-dir "$target_dir" >&2

out="$target_dir/release/$library"
test -f "$out" || { echo "build.sh: cargo produced no $library" >&2; exit 1; }

if [ "$(uname -s)" = Linux ]; then
  # Its own assignment, so a missing or failing readelf stops the build
  # (set -e) rather than passing an empty list through the check.
  dynamic=$(readelf -d "$out")
  needed=$(printf '%s\n' "$dynamic" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')
  test -n "$needed" || { echo "build.sh: readelf found no NEEDED entries in $library" >&2; exit 1; }
  echo "build.sh: $library needs: $(echo $needed)" >&2
  if echo "$needed" | grep -q 'libstdc++'; then
    echo "build.sh: $library still needs libstdc++" >&2
    exit 1
  fi
fi

echo "$out"
