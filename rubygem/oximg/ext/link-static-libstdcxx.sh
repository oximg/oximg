#!/bin/sh
# Linker for the musl probe library build (see build.sh): the system
# cc, with jpegli's -lstdc++ turned into the static archive, so the
# library does not need libstdc++.so.6 at run time.
for arg; do
  shift
  [ "$arg" = "-lstdc++" ] && arg="-l:libstdc++.a"
  set -- "$@" "$arg"
done
exec cc "$@"
