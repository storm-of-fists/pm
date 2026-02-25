find_package(Threads REQUIRED)

add_executable(pm_tests test.cpp)
target_compile_options(pm_tests PRIVATE -Wall -Wextra -Wpedantic)
target_include_directories(pm_tests PRIVATE
    ${CMAKE_SOURCE_DIR}/src
    ${CMAKE_SOURCE_DIR}/src/pm
    ${CMAKE_SOURCE_DIR}/src/examples/hellfire
)
target_link_libraries(pm_tests PRIVATE Threads::Threads)

enable_testing()
add_test(NAME pm_tests COMMAND pm_tests)
