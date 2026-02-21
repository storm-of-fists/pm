# PM — Process Manager build system
# Usage:
#   make              Build everything (dev)
#   make dev          Build hellfire (debug, fast compile)
#   make test         Build and run tests
#   make release      Build hellfire (optimized)
#   make clean        Remove build artifacts

CXX       := g++
CXXFLAGS  := -std=c++17 -Wall -Wextra -Wpedantic
INCLUDES  := -Isrc

# SDL2 (only needed for hellfire)
SDL_CFLAGS  := $(shell sdl2-config --cflags 2>/dev/null)
SDL_LDFLAGS := $(shell sdl2-config --libs 2>/dev/null)

# Platform networking
UNAME := $(shell uname -s)
ifeq ($(UNAME),Linux)
    NET_LDFLAGS :=
endif
ifeq ($(UNAME),Darwin)
    NET_LDFLAGS :=
endif
ifneq (,$(findstring MINGW,$(UNAME))$(findstring MSYS,$(UNAME)))
    NET_LDFLAGS := -lws2_32
endif

# Directories
BUILD_DIR   := build
SRC_DIR     := src
TEST_DIR    := tests
EXAMPLE_DIR := examples/hellfire

# Sources
HEADERS     := $(wildcard $(SRC_DIR)/*.hpp)
TEST_SRC    := $(TEST_DIR)/test.cpp
HELLFIRE_SRC:= $(EXAMPLE_DIR)/hellfire.cpp

# Targets
TEST_BIN    := $(BUILD_DIR)/test
DEV_BIN     := $(BUILD_DIR)/hellfire_dev
RELEASE_BIN := $(BUILD_DIR)/hellfire

# ── Profiles ─────────────────────────────────────────────────────────────────

DEV_FLAGS     := -O0 -g -DDEBUG
RELEASE_FLAGS := -O3 -DNDEBUG -march=native -flto

# ── Rules ────────────────────────────────────────────────────────────────────

.PHONY: all dev test release clean run run-release

all: dev test

$(BUILD_DIR):
	@mkdir -p $(BUILD_DIR)

# Dev build — fast compile, debug symbols, assertions on
dev: $(DEV_BIN)

$(DEV_BIN): $(HELLFIRE_SRC) $(HEADERS) | $(BUILD_DIR)
	$(CXX) $(CXXFLAGS) $(DEV_FLAGS) $(INCLUDES) $(SDL_CFLAGS) -o $@ $< $(SDL_LDFLAGS) $(NET_LDFLAGS)
	@echo "  Built: $@ (dev)"

# Test — build and run
test: $(TEST_BIN)
	@echo "──────────────────────────────"
	@$(TEST_BIN)

$(TEST_BIN): $(TEST_SRC) $(HEADERS) | $(BUILD_DIR)
	$(CXX) $(CXXFLAGS) $(DEV_FLAGS) $(INCLUDES) -o $@ $<
	@echo "  Built: $@ (test)"

# Release — full optimizations, LTO, no debug
release: $(RELEASE_BIN)

$(RELEASE_BIN): $(HELLFIRE_SRC) $(HEADERS) | $(BUILD_DIR)
	$(CXX) $(CXXFLAGS) $(RELEASE_FLAGS) $(INCLUDES) $(SDL_CFLAGS) -o $@ $< $(SDL_LDFLAGS) $(NET_LDFLAGS)
	@echo "  Built: $@ (release)"

# Convenience runners
run: dev
	$(DEV_BIN)

run-release: release
	$(RELEASE_BIN)

clean:
	rm -rf $(BUILD_DIR)