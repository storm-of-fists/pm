# PM — Process Manager build system
# Usage:
#   make              Build everything (dev)
#   make dev          Build client + server (debug)
#   make server       Build server only (no SDL)
#   make client       Build client only
#   make test         Build and run tests
#   make release      Build client + server (optimized)
#   make clean        Remove build artifacts

CXX       := g++
CXXFLAGS  := -std=c++17 -Wall -Wextra -Wpedantic
INCLUDES  := -Isrc -Iexamples/hellfire

# SDL2 (client only)
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
HEADERS      := $(wildcard $(SRC_DIR)/*.hpp) $(EXAMPLE_DIR)/hellfire_common.hpp
SERVER_SRC   := $(EXAMPLE_DIR)/hellfire_server.cpp
CLIENT_SRC   := $(EXAMPLE_DIR)/hellfire_client.cpp
TEST_SRC     := $(TEST_DIR)/test.cpp

# Profiles
DEV_FLAGS     := -O0 -g -DDEBUG
RELEASE_FLAGS := -O3 -DNDEBUG -march=native -flto

# ── Rules ────────────────────────────────────────────────────────────────────

.PHONY: all dev server client test release clean run

all: dev test

$(BUILD_DIR):
	@mkdir -p $(BUILD_DIR)

dev: $(BUILD_DIR)/hellfire_server $(BUILD_DIR)/hellfire_client
	@echo "  Built: client + server (dev)"

server: $(BUILD_DIR)/hellfire_server

client: $(BUILD_DIR)/hellfire_client

# Server — no SDL
$(BUILD_DIR)/hellfire_server: $(SERVER_SRC) $(HEADERS) | $(BUILD_DIR)
	$(CXX) $(CXXFLAGS) $(DEV_FLAGS) $(INCLUDES) -o $@ $< $(NET_LDFLAGS)

# Client — needs SDL
$(BUILD_DIR)/hellfire_client: $(CLIENT_SRC) $(HEADERS) | $(BUILD_DIR)
	$(CXX) $(CXXFLAGS) $(DEV_FLAGS) $(INCLUDES) $(SDL_CFLAGS) -o $@ $< $(SDL_LDFLAGS) $(NET_LDFLAGS)

# Test
test: $(BUILD_DIR)/test
	@echo "──────────────────────────────"
	@$(BUILD_DIR)/test

$(BUILD_DIR)/test: $(TEST_SRC) $(HEADERS) | $(BUILD_DIR)
	$(CXX) $(CXXFLAGS) $(DEV_FLAGS) $(INCLUDES) -o $@ $<

# Release
release: $(BUILD_DIR)/hellfire_server_rel $(BUILD_DIR)/hellfire_client_rel
	@echo "  Built: client + server (release)"

$(BUILD_DIR)/hellfire_server_rel: $(SERVER_SRC) $(HEADERS) | $(BUILD_DIR)
	$(CXX) $(CXXFLAGS) $(RELEASE_FLAGS) $(INCLUDES) -o $@ $< $(NET_LDFLAGS)

$(BUILD_DIR)/hellfire_client_rel: $(CLIENT_SRC) $(HEADERS) | $(BUILD_DIR)
	$(CXX) $(CXXFLAGS) $(RELEASE_FLAGS) $(INCLUDES) $(SDL_CFLAGS) -o $@ $< $(SDL_LDFLAGS) $(NET_LDFLAGS)

run: dev
	$(BUILD_DIR)/hellfire_client

clean:
	rm -rf $(BUILD_DIR)