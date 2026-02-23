add_executable(pm_tests test.cpp)
target_include_directories(pm_tests PRIVATE
    ${CMAKE_SOURCE_DIR}/src
    ${CMAKE_SOURCE_DIR}/examples/hellfire
)

enable_testing()
add_test(NAME pm_tests COMMAND pm_tests)