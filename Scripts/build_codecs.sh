#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Builds avif.dll and jxl*.dll with vcpkg (static deps, static CRT, release only) into K:/cache/br-codecs.
# Needs about 6 GB free on K: (sources + tools). Copy installed/x64-windows-br/bin/{avif,jxl,jxl_cms,jxl_threads}.dll
# to src/BetterRecall.Agent/Native/ and rebuild codecs.LICENSE.txt from installed/x64-windows-br/share/<port>/copyright.
# A port version change means re-checking the struct offsets in Native/Avif.cs and Native/Jxl.cs.
set -euo pipefail
ROOT=/k/cache/br-codecs
mkdir -p "$ROOT" "$ROOT/triplets" "$ROOT/downloads" "$ROOT/bincache"
cd "$ROOT"
VCPKG_COMMIT=2ec0f1747d8186a5c9babe276e2ad57cd2b12d2a   # 2026-11-11: libavif 1.4.2, aom 3.15.3, dav1d 1.5.4, libjxl 0.12.0
if [ ! -d vcpkg ]; then
  git clone https://github.com/microsoft/vcpkg vcpkg
fi
cd vcpkg
git checkout --quiet "$VCPKG_COMMIT"
if [ ! -f vcpkg.exe ]; then
  cmd //c "K:\cache\br-codecs\vcpkg\bootstrap-vcpkg.bat -disableMetrics"
fi
cat > "$ROOT/triplets/x64-windows-br.cmake" <<'TRIPLET'
set(VCPKG_TARGET_ARCHITECTURE x64)
set(VCPKG_CRT_LINKAGE static)
set(VCPKG_LIBRARY_LINKAGE static)
if(PORT MATCHES "^(libavif|libjxl)$")
  set(VCPKG_LIBRARY_LINKAGE dynamic)
endif()
set(VCPKG_BUILD_TYPE release)
TRIPLET
export VCPKG_DISABLE_METRICS=1
export VCPKG_DOWNLOADS="K:\\cache\\br-codecs\\downloads"
export VCPKG_DEFAULT_BINARY_CACHE="K:\\cache\\br-codecs\\bincache"
./vcpkg.exe install "libavif[core,aom,dav1d]" libjxl --triplet x64-windows-br \
  --overlay-triplets="K:\\cache\\br-codecs\\triplets" --clean-after-build --x-install-root="K:\\cache\\br-codecs\\installed"
echo BUILD-DONE
ls -la "$ROOT/installed/x64-windows-br/bin"
