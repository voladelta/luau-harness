cmake_minimum_required(VERSION 3.28)
project(hx LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
if(NOT CMAKE_BUILD_TYPE)
  set(CMAKE_BUILD_TYPE Release)
endif()

if(NOT CMAKE_SYSTEM_NAME STREQUAL "Linux")
  message(FATAL_ERROR "hx is Linux only: its sandbox is Landlock")
endif()

# Luau is the only vendored dependency, pinned to a release. Only the VM and the compiler are built.
include(FetchContent)
set(LUAU_BUILD_CLI OFF CACHE BOOL "" FORCE)
set(LUAU_BUILD_TESTS OFF CACHE BOOL "" FORCE)
FetchContent_Declare(
  luau
  GIT_REPOSITORY https://github.com/luau-lang/luau.git
  GIT_TAG 0.747
  GIT_SHALLOW TRUE
  EXCLUDE_FROM_ALL)
FetchContent_MakeAvailable(luau)

find_package(CURL REQUIRED)

# The default loop is compiled into the binary, so hx works in a directory that has no .hx/ yet and still
# starts when a project's own loop is broken.
file(READ ${CMAKE_CURRENT_SOURCE_DIR}/lua/loop.luau HX_LOOP_SOURCE)
configure_file(src/embedded.h.in embedded.h @ONLY)
set_property(DIRECTORY APPEND PROPERTY CMAKE_CONFIGURE_DEPENDS lua/loop.luau)

add_executable(hx src/main.cpp src/host.cpp src/json.cpp src/sandbox.cpp)
target_include_directories(hx PRIVATE ${CMAKE_CURRENT_BINARY_DIR})
target_link_libraries(hx PRIVATE Luau.Compiler Luau.VM CURL::libcurl)
target_compile_options(hx PRIVATE -Wall -Wextra -Wpedantic -Wshadow -Wconversion)

install(TARGETS hx RUNTIME DESTINATION bin)

enable_testing()
add_test(NAME json COMMAND hx run ${CMAKE_CURRENT_SOURCE_DIR}/tests/json_test.luau)
add_test(NAME tools COMMAND hx run ${CMAKE_CURRENT_SOURCE_DIR}/tests/tools_test.luau)
add_test(NAME agent COMMAND python3 ${CMAKE_CURRENT_SOURCE_DIR}/tests/agent_test.py $<TARGET_FILE:hx>)
set_tests_properties(json tools PROPERTIES WORKING_DIRECTORY ${CMAKE_CURRENT_BINARY_DIR})
