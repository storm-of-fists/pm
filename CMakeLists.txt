cmake_minimum_required(VERSION 3.16)
project(pm CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_RUNTIME_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR})

# compile_commands.json for clangd / VSCode IntelliSense
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

add_compile_options(-Wall -Wextra -Wpedantic)

# SDL2 — modern cmake config (ships with SDL2 >= 2.0.12)
find_package(SDL2 REQUIRED)

# SDL2_image — no cmake config; use pkg-config
find_package(PkgConfig REQUIRED)
pkg_check_modules(SDL2_IMAGE REQUIRED IMPORTED_TARGET SDL2_image)

# Platform networking
if(WIN32)
    set(NET_LIBS ws2_32)
endif()

add_subdirectory(examples/hellfire)
add_subdirectory(tests)