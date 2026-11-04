#!/bin/bash
#
# Build cpp-worker, the C++ reference as a batch worker for
# `solver-bench engines --cpp-worker`.
#
#   bench/comparison/build-cpp-worker.sh <reference checkout> [OUT]
#
# The checkout is macroxue/bridge-solver or a fork of it; only its solver.cc
# and solver.h are used. The build follows the reference makefile's clang
# recipe: -O3 -march=native -fno-stack-protector, with PGO trained on
# deals/hard/deal.8 solved for every strain and lead, which is what its
# `solver.p` target runs.
#
# Works with clang++ on Linux, macOS, and Windows (LLVM's clang++ with the
# MSVC libraries; Git Bash is enough to run this script there).
#
# For comparing build variants:
#   EXTRA_OPTS="-falign-loops=32"   extra compiler flags, both PGO passes
#   EXTRA_SOURCES="a.cc b.cc"       more sources to compile in
#   TRAIN=FILE                      PGO training input in cpp-worker's own
#                                   protocol ("<id> <strain> <PBN>" lines)
#                                   instead of deal.8
set -euo pipefail

SRC=${1:?usage: build-cpp-worker.sh <reference checkout> [OUT]}
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${2:-$HERE/cpp-build/cpp-worker}
CXX=${CXX:-clang++}
[ -f "$SRC/solver.cc" ] || { echo "no solver.cc in $SRC" >&2; exit 1; }

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) EXE=.exe ;;
  *) EXE= ;;
esac
OUT=${OUT%.exe}$EXE
mkdir -p "$(dirname "$OUT")"

# GCC (CXX=g++, e.g. MinGW-w64 on Windows) gets the makefile's GCC recipe;
# anything else is taken to be clang.
if "$CXX" --version 2>/dev/null | head -1 | grep -q -E 'g\+\+|GCC'; then
  IS_GCC=1
else
  IS_GCC=
  LLVM_PROFDATA=${LLVM_PROFDATA:-$("$CXX" -print-prog-name=llvm-profdata)}
  command -v "$LLVM_PROFDATA" >/dev/null 2>&1 || [ -x "$LLVM_PROFDATA" ] \
    || LLVM_PROFDATA=$(command -v llvm-profdata) \
    || { echo "llvm-profdata not found" >&2; exit 1; }
fi

OPTS=(-std=c++17 -O3 -march=native -fno-stack-protector -I"$SRC")
if [ -n "$IS_GCC" ]; then
  # MinGW has S_ISCHR; link its runtime in so the worker runs from anywhere.
  if [ -n "$EXE" ]; then OPTS+=(-static); fi
elif [ -n "$EXE" ]; then
  # The MSVC runtime has fstat but no S_ISCHR, which solver.cc uses to decide
  # whether to colour suit symbols. It never matters here: output is a pipe.
  OPTS+=(-D_CRT_SECURE_NO_WARNINGS "-DS_ISCHR(m)=(((m)&_S_IFMT)==_S_IFCHR)")
fi
# shellcheck disable=SC2206  # EXTRA_OPTS is a list of flags
OPTS+=(${EXTRA_OPTS:-})
SOURCES=("$SRC/solver.cc" "$HERE/cpp-worker.cc")
# shellcheck disable=SC2206
SOURCES+=(${EXTRA_SOURCES:-})

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT

# deals/hard/deal.8, the reference's PGO training deal, as cpp-worker input:
# every strain in the CLI's order, each solved for all four leads.
DEAL8='N:42.AQT63.K52.T84 AJ65.42.J963.KQJ Q973..AT74.A9632 KT8.KJ9875.Q8.75'
deal8() { local i=0; for s in N S H D C; do echo "$i $s $DEAL8"; i=$((i+1)); done; }
train() { if [ -n "${TRAIN:-}" ]; then cat "$TRAIN"; else deal8; fi; }

if [ -n "$IS_GCC" ]; then
  # GCC names each profile after its object file, so both passes compile to
  # the same object names and the second finds the first's .gcda files.
  objects() {
    local i=0
    OBJS=()
    for src in "${SOURCES[@]}"; do
      "$CXX" "${OPTS[@]}" "$@" -c "$src" -o "$work/o$i.o"
      OBJS+=("$work/o$i.o")
      i=$((i+1))
    done
  }
  objects -fprofile-generate
  "$CXX" "${OPTS[@]}" -fprofile-generate -o "$work/pgen$EXE" "${OBJS[@]}"
  train | "$work/pgen$EXE"
  objects -fprofile-use -Wno-missing-profile
  "$CXX" "${OPTS[@]}" -o "$OUT" "${OBJS[@]}"
else
  "$CXX" "${OPTS[@]}" -fprofile-generate="$work/prof" -o "$work/pgen$EXE" "${SOURCES[@]}"
  train | "$work/pgen$EXE"
  "$LLVM_PROFDATA" merge -o "$work/merged.profdata" "$work"/prof/*.profraw
  "$CXX" "${OPTS[@]}" -fprofile-use="$work/merged.profdata" -Wno-profile-instr-out-of-date \
    -o "$OUT" "${SOURCES[@]}"
fi
echo "deal.8 with the PGO build:"
deal8 | "$OUT"
echo "built $OUT ($(git -C "$SRC" rev-parse --short HEAD 2>/dev/null || echo unknown))"
