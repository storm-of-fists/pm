find_package(Threads REQUIRED)

add_executable(pm_tests test.cpp)
target_compile_options(pm_tests PRIVATE ${PM_WARNINGS} -UNDEBUG)
target_include_directories(pm_tests PRIVATE
    ${CMAKE_SOURCE_DIR}/src
    ${CMAKE_SOURCE_DIR}/src/pm
    ${CMAKE_SOURCE_DIR}/src/examples/hellfire
)
target_link_libraries(pm_tests PRIVATE Threads::Threads)

add_executable(pm_bench bench.cpp)
target_compile_options(pm_bench PRIVATE ${PM_WARNINGS} -UNDEBUG)
target_include_directories(pm_bench PRIVATE
    ${CMAKE_SOURCE_DIR}/src
    ${CMAKE_SOURCE_DIR}/src/pm
)
target_link_libraries(pm_bench PRIVATE Threads::Threads)

enable_testing()
add_test(NAME pm_tests COMMAND pm_tests)
