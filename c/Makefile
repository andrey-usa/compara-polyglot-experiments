# Two compilers, because the gap between them is a finding this port exists to
# show: on 1M rows gcc takes 5.34s and clang 4.77s from the same source.
#
# -pthread is for both paths; the text one divides inside the file and the
# columnar one by column.
#
# `-march=native` under `-Werror` is a hazard, and it fired: on a runner whose
# CPU advertises avx10.1-256, clang 18 refuses with
#
#   invalid feature combination: +avx10.1-256; will be promoted to avx10.1-512
#
# before it reads a line of this code. That is the driver describing the host,
# not a diagnostic about the source, and turning it into a build failure means
# the port stops compiling when a cloud provider refreshes its fleet. The
# warning is suppressed if the compiler in use knows the flag, and left alone if
# it does not, so this stays quiet on gcc and on older clangs.
#
# `-march=native` is itself probed, for the same reason and a different host:
# Apple clang on arm64 rejects the flag outright, so a Mac could not build this
# at all. Asking the compiler whether it takes the flag costs one preprocessor
# run at configure time and means the Makefile does not have to know which
# compiler on which architecture it is talking to.
#
# On Windows two things change and nothing else does. gcc appends `.exe` whether
# or not it is asked, so a target named `csvdiff` would never appear to be built
# and make would relink on every run -- hence $(EXE). And `-pthread` goes: this
# port's threads are CreateThread there (see the top of parallel.c), so linking
# winpthreads would add a DLL to the binary for nothing.
CC     ?= cc
ifeq ($(OS),Windows_NT)
EXE     := .exe
endif
ifneq (,$(filter MINGW% MSYS% CYGWIN%,$(shell uname -s 2>/dev/null)))
EXE     := .exe
endif
ifeq ($(EXE),)
PTHREAD := -pthread
endif
NATIVE_QUIRK := $(shell $(CC) -Werror -Wno-invalid-feature-combination -E - </dev/null >/dev/null 2>&1 \
                    && echo -Wno-invalid-feature-combination)
NATIVE_ARCH  := $(shell $(CC) -march=native -E - </dev/null >/dev/null 2>&1 && echo -march=native)
CFLAGS ?= -std=c11 -O2 $(NATIVE_ARCH) -Wall -Wextra -Werror $(PTHREAD) $(NATIVE_QUIRK)
LDFLAGS ?= $(PTHREAD)
BIN     = csvdiff$(EXE)
GENBIN  = gen-data$(EXE)
# Two programs live here, each with its own main, so the sources are named
# rather than globbed. `c/*.c` would put both mains in one link -- which is
# exactly how CI broke when the generator arrived.
ENGINE  = csvdiff.c parquet.c pqdiff.c parallel.c
GEN     = gen-data.c pqwrite.c parallel.c
OBJ     = csvdiff.o parquet.o pqdiff.o parallel.o

all: $(BIN) $(GENBIN)

$(BIN): $(OBJ)
	$(CC) $(CFLAGS) -o $@ $(OBJ) $(LDFLAGS)

csvdiff.o:  csvdiff.c pqdiff.h parquet.h parallel.h win32.h
parquet.o:  parquet.c parquet.h
pqdiff.o:   pqdiff.c pqdiff.h parquet.h parallel.h win32.h
parallel.o: parallel.c parallel.h

# The generator writes this port's own fixtures and benchmark inputs, byte for
# byte the same files the other generators write. Having it here is what lets a
# check run without a second toolchain built first.
$(GENBIN): $(GEN) pqwrite.h parallel.h win32.h
	$(CC) $(CFLAGS) -o $@ $(GEN)

# `make gen-data` is what every script and workflow here asks for by name. Off
# Windows that already is the file; on Windows the file is `gen-data.exe`, so
# the plain name becomes an alias for it -- and only there, because defining it
# where the two names are the same is a rule that depends on itself.
ifneq ($(EXE),)
gen-data: $(GENBIN)
.PHONY: gen-data
endif

# The engine under AddressSanitizer and UndefinedBehaviorSanitizer. It is a
# target rather than a command in the workflow so that the source list has one
# home: a second copy in CI is a copy that goes stale, and did.
asan: $(ENGINE)
	$(CC) -std=c11 -O1 -g -fsanitize=address,undefined -Wall -Wextra $(PTHREAD) \
	    $(NATIVE_QUIRK) -o csvdiff-asan $(ENGINE)

clean:
	rm -f $(BIN) $(GENBIN) csvdiff-asan $(OBJ)

.PHONY: all asan clean
