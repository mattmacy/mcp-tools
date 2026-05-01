#!/usr/bin/env bash
# Build patched clangd from a local llvm-project checkout and install
# to /usr/local/bin/clangd-patched.
#
# The patch adds --background-index-memory-limit=<bytes>[K|M|G] which
# caps resident size of the merged background index by evicting the
# least-recently-updated file's slabs from FileSymbols. Disk shards
# remain authoritative.
#
# Inputs (env-overridable):
#   LLVM_ROOT      Path to a checked-out llvm-project source tree.
#                  Required.
#   PATCH_DIR      Directory containing the *.patch files to apply.
#                  Required if patches have not already been applied
#                  to LLVM_ROOT.
#   BUILD_DIR      Where to build (incremental ninja). Defaults to
#                  $LLVM_ROOT/.build-clangd-patched.
#   INSTALL_PATH   Final install path. Default
#                  /usr/local/bin/clangd-patched.
set -euo pipefail

LLVM_ROOT="${LLVM_ROOT:-}"
if [[ -z "${LLVM_ROOT}" ]]; then
  echo "error: set LLVM_ROOT to a checked-out llvm-project source tree" >&2
  exit 2
fi
BUILD_DIR="${BUILD_DIR:-${LLVM_ROOT}/.build-clangd-patched}"
INSTALL_PATH="${INSTALL_PATH:-/usr/local/bin/clangd-patched}"
PATCH_DIR="${PATCH_DIR:-}"

if [[ ! -d "${LLVM_ROOT}/clang-tools-extra/clangd" ]]; then
  echo "error: ${LLVM_ROOT}/clang-tools-extra/clangd missing — bad LLVM_ROOT" >&2
  exit 1
fi

if ! command -v ninja >/dev/null; then
  echo "error: ninja not on PATH" >&2
  exit 1
fi

# Apply patches if the marker is missing.
PATCH_MARKER="background-index-memory-limit"
if ! grep -q "$PATCH_MARKER" "${LLVM_ROOT}/clang-tools-extra/clangd/tool/ClangdMain.cpp"; then
  if [[ -n "$PATCH_DIR" && -d "$PATCH_DIR" ]]; then
    echo "applying patches from ${PATCH_DIR}…"
    pushd "$LLVM_ROOT" >/dev/null
    for patch in "$PATCH_DIR"/*.patch; do
      [[ -f "$patch" ]] || continue
      git apply --check "$patch" && git apply "$patch"
      echo "  applied: $(basename "$patch")"
    done
    popd >/dev/null
  else
    echo "error: patch marker '$PATCH_MARKER' missing from ClangdMain.cpp and PATCH_DIR is unset/missing — cannot continue" >&2
    exit 1
  fi
fi

mkdir -p "$BUILD_DIR"

if [[ ! -f "${BUILD_DIR}/CMakeCache.txt" ]]; then
  cmake -B "$BUILD_DIR" -G Ninja -S "${LLVM_ROOT}/llvm" \
    -DCMAKE_BUILD_TYPE=Release \
    -DLLVM_ENABLE_PROJECTS="clang;clang-tools-extra" \
    -DLLVM_TARGETS_TO_BUILD=X86 \
    -DLLVM_USE_LINKER=lld \
    -DCMAKE_C_COMPILER=clang-19 \
    -DCMAKE_CXX_COMPILER=clang++-19 \
    -DLLVM_PARALLEL_LINK_JOBS=2
fi

ninja -C "$BUILD_DIR" clangd

if [[ -x "${BUILD_DIR}/bin/clangd" ]]; then
  echo "installing ${BUILD_DIR}/bin/clangd to ${INSTALL_PATH} (sudo)…"
  sudo install -m755 "${BUILD_DIR}/bin/clangd" "$INSTALL_PATH"
  echo "installed: $("$INSTALL_PATH" --version | head -1)"
  echo "memory-limit flag check:"
  "$INSTALL_PATH" --help-list-hidden 2>&1 | grep -F "background-index-memory-limit" || \
    "$INSTALL_PATH" --help 2>&1 | grep -F "background-index-memory-limit" || \
    echo "  (flag not visible in --help; will be set at launch)"
else
  echo "error: ninja did not produce ${BUILD_DIR}/bin/clangd" >&2
  exit 1
fi
