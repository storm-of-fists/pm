cmake_minimum_required(VERSION 3.16)
project(pm CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_RUNTIME_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR})

# compile_commands.json for clangd / VSCode IntelliSense
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

# SDL3 + SDL3_image — built from source via FetchContent
include(FetchContent)

FetchContent_Declare(SDL3
    GIT_REPOSITORY https://github.com/libsdl-org/SDL.git
    GIT_TAG        release-3.4.2
    GIT_SHALLOW    TRUE)
FetchContent_MakeAvailable(SDL3)

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