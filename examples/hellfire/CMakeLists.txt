set(PM_INCLUDES
    ${CMAKE_SOURCE_DIR}/src
    ${CMAKE_CURRENT_SOURCE_DIR}
)

# ── Server (no SDL) ──────────────────────────────────────────────────────────
add_executable(hellfire_server hellfire_server.cpp)
target_include_directories(hellfire_server PRIVATE ${PM_INCLUDES})
target_link_libraries(hellfire_server PRIVATE ${NET_LIBS})

# ── Client (SDL2 + SDL2_image) ───────────────────────────────────────────────
add_executable(hellfire_client hellfire_client.cpp)
target_include_directories(hellfire_client PRIVATE ${PM_INCLUDES})
target_link_libraries(hellfire_client PRIVATE
    SDL2::SDL2
    PkgConfig::SDL2_IMAGE
    ${NET_LIBS}
)

# Copy resources/ next to the client binary after each build
add_custom_command(TARGET hellfire_client POST_BUILD
    COMMAND ${CMAKE_COMMAND} -E copy_directory
        ${CMAKE_CURRENT_SOURCE_DIR}/resources
        $<TARGET_FILE_DIR:hellfire_client>/resources
    COMMENT "Copying hellfire resources"
)

# ── Example mod (hot-reload demo) ───────────────────────────────────────────
add_library(example_mod MODULE mods/example_mod.cpp)
target_include_directories(example_mod PRIVATE ${PM_INCLUDES})
set_target_properties(example_mod PROPERTIES PREFIX "")

# Copy mod .so next to the client binary after each build
add_custom_command(TARGET example_mod POST_BUILD
    COMMAND ${CMAKE_COMMAND} -E make_directory
        $<TARGET_FILE_DIR:hellfire_client>/mods
    COMMAND ${CMAKE_COMMAND} -E copy
        $<TARGET_FILE:example_mod>
        $<TARGET_FILE_DIR:hellfire_client>/mods/example_mod.so
    COMMENT "Copying example_mod to mods/"
)