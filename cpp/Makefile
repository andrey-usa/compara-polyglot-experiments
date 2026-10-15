# Two compilers, because the point of this port is to be the reference an
# ordinary C++ programmer would write and check against. The default is the
# generic name rather than `g++`: on a Mac `g++` is a shim for Apple clang when
# it exists at all, and `c++` is the name every one of them answers to.
# `-Wno-invalid-feature-combination` is not a style choice: GitHub has begun
# rolling out runners whose CPUs advertise AVX10.1, and on those `-march=native`
# makes clang warn that `+avx10.1-256` will be promoted to `avx10.1-512`. Under
# `-Werror` that is a build failure, so the benchmark started failing on some
# runners and passing on others depending which machine it landed on. The
# promotion is harmless -- it is clang telling us what it decided, not a problem
# with the code -- and every other warning stays fatal.
# Both flags are probed rather than assumed, the same way the C Makefile does it.
# `-Wno-invalid-feature-combination` is a clang spelling that gcc does not know,
# and `-march=native` is rejected outright by Apple clang on arm64 -- so hard
# coding either one means a host that cannot build this at all.
CXX      ?= c++
# gcc appends `.exe` on Windows whether or not it is asked, so a target named
# `build/csvdiff` would never appear to exist and make would relink on every
# run. `-pthread` stays there, unlike in the C port: this one's threads are
# std::thread, and mingw-w64's default threads model implements them over
# winpthreads.
ifeq ($(OS),Windows_NT)
EXE      := .exe
endif
ifneq (,$(filter MINGW% MSYS% CYGWIN%,$(shell uname -s 2>/dev/null)))
EXE      := .exe
endif
NATIVE_QUIRK := $(shell $(CXX) -Werror -Wno-invalid-feature-combination -E -x c++ - </dev/null >/dev/null 2>&1 \
                    && echo -Wno-invalid-feature-combination)
NATIVE_ARCH  := $(shell $(CXX) -march=native -E -x c++ - </dev/null >/dev/null 2>&1 && echo -march=native)
CXXFLAGS ?= -std=c++20 -O2 $(NATIVE_ARCH) -Wall -Wextra -Wpedantic -Werror \
            $(NATIVE_QUIRK) -pthread
SRC       = src/csvdiff.cpp src/pqdiff.cpp src/parquet.cpp src/json.cpp src/main.cpp
BIN       = build/csvdiff$(EXE)
GEN       = build/gen-data$(EXE)

all: $(BIN)

$(BIN): $(SRC) src/csvdiff.hpp src/pqdiff.hpp src/parquet.hpp
	@mkdir -p build
	$(CXX) $(CXXFLAGS) -o $@ $(SRC)

# The scanner as its own binary rather than a runtime switch: a benchmark of an
# instruction set should not be measuring a function pointer, and each of these
# is what you would ship for that target. `scanners` needs a CPU that has the
# instructions; the AVX-512 build will not start on one that does not.
# No `csvdiff-swar` here. It was built from the same sources with the same flags
# and no define, which is exactly how `build/csvdiff` is built -- the two came
# out byte for byte identical, so the matrix row it fed was a build measured
# against itself. The plain build takes the eight-byte step and is the SWAR
# baseline these two are compared against.
scanners: build/csvdiff-avx2$(EXE) build/csvdiff-avx512$(EXE)

build/csvdiff-avx2$(EXE): $(SRC) src/csvdiff.hpp src/pqdiff.hpp src/parquet.hpp
	@mkdir -p build
	$(CXX) $(CXXFLAGS) -DCSVDIFF_SCAN_AVX2 -mavx2 -o $@ $(SRC)

build/csvdiff-avx512$(EXE): $(SRC) src/csvdiff.hpp src/pqdiff.hpp src/parquet.hpp
	@mkdir -p build
	$(CXX) $(CXXFLAGS) -DCSVDIFF_SCAN_AVX512 -mavx512f -mavx512bw -o $@ $(SRC)

# The engine under AddressSanitizer and UndefinedBehaviorSanitizer, as C has
# had since it was added there. -O1 and no -march=native: the point is the
# checks, and a native build cannot run on a machine other than the one that made
# it. A report exits 99 (ASan) or 98 (UBSan) instead of the default 1, because 1
# already means "differences found" -- see scripts/fuzz_diff.py and sanitizers.yml.
asan: build/csvdiff-asan$(EXE)

build/csvdiff-asan$(EXE): $(SRC) src/csvdiff.hpp src/pqdiff.hpp src/parquet.hpp
	@mkdir -p build
	$(CXX) -std=c++20 -O1 -g -fno-omit-frame-pointer -fsanitize=address,undefined \
	    -fno-sanitize-recover=undefined -Wall -Wextra -Wpedantic $(NATIVE_QUIRK) -pthread \
	    -o $@ $(SRC)

# The benchmark data generator. Writes the same bytes as the five other
# generators; it is here because it is the only one fast enough to keep up with
# the disk. See tools/gen_data.cpp.
gen-data: $(GEN)

$(GEN): tools/gen_data.cpp tools/pq_write.cpp tools/pq_write.hpp
	@mkdir -p build
	$(CXX) $(CXXFLAGS) -o $@ tools/gen_data.cpp tools/pq_write.cpp

# A verification tool for the Parquet reader: dumps a file's schema, or one
# column's values as text, so what the reader decoded can be diffed against
# what wrote the file.
pq-dump: build/pq-dump$(EXE)

build/pq-dump$(EXE): tools/pq_dump.cpp src/parquet.cpp src/parquet.hpp
	@mkdir -p build
	$(CXX) $(CXXFLAGS) -o $@ tools/pq_dump.cpp src/parquet.cpp

clean:
	rm -rf build

.PHONY: all asan gen-data scanners pq-dump clean
