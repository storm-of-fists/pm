cmake_minimum_required(VERSION 3.16)
project(pm CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_RUNTIME_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR})

# compile_commands.json for clangd / VSCode IntelliSense
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

# Strict warnings — used by all pm targets (not applied to FetchContent deps)
set(PM_WARNINGS -Wall -Wextra -Wpedantic -Werror)

# ccache — speeds up recompilation dramatically
find_program(CCACHE ccache)
if(CCACHE)
    set(CMAKE_CXX_COMPILER_LAUNCHER ${CCACHE})
    message(STATUS "Using ccache: ${CCACHE}")
endif()

# SDL3 + SDL3_image — built from source via FetchContent
include(FetchContent)
set(FETCHCONTENT_UPDATES_DISCONNECTED ON CACHE BOOL "Skip git checks after first clone")

FetchContent_Declare(SDL3
    GIT_REPOSITORY https://github.com/libsdl-org/SDL.git
    GIT_TAG        release-3.4.2
    GIT_SHALLOW    TRUE)
FetchContent_MakeAvailable(SDL3)

# Only build image formats we actually use (PNG) — skip heavy codecs
set(SDLIMAGE_AVIF OFF CACHE BOOL "" FORCE)
set(SDLIMAGE_AVIF_SAVE OFF CACHE BOOL "" FORCE)
set(SDLIMAGE_JXL  OFF CACHE BOOL "" FORCE)
set(SDLIMAGE_TIF  OFF CACHE BOOL "" FORCE)
set(SDLIMAGE_WEBP OFF CACHE BOOL "" FORCE)
set(SDLIMAGE_WEBP_SAVE OFF CACHE BOOL "" FORCE)
set(SDLIMAGE_JPG  OFF CACHE BOOL "" FORCE)
set(SDLIMAGE_JPG_SAVE OFF CACHE BOOL "" FORCE)

FetchContent_Declare(SDL3_image
    GIT_REPOSITORY https://github.com/libsdl-org/SDL_image.git
    GIT_TAG        release-3.4.0
    GIT_SHALLOW    TRUE)
FetchContent_MakeAvailable(SDL3_image)

# Platform networking
if(WIN32)
    set(NET_LIBS ws2_32)
endif()

enable_testing()

add_subdirectory(src/examples/hellfire)
add_subdirectory(src/tests)