cmake_minimum_required(VERSION 3.28)
project(hx VERSION 0.1.0 DESCRIPTION "A small coding-agent host: a Luau VM, five tools, one sandbox" LANGUAGES CXX)

if(NOT CMAKE_SYSTEM_NAME STREQUAL "Linux")
  message(FATAL_ERROR "hx is Linux only: its sandbox is Landlock")
endif()

option(HX_WERROR "Treat compiler warnings as errors" OFF)
set(HX_SANITIZE "" CACHE STRING "Sanitizers to build with, for example: address,undefined")

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
  set(CMAKE_BUILD_TYPE Release CACHE STRING "Build type" FORCE)
endif()

include(CheckPIESupported)
check_pie_supported()

# Sanitizers have to cover the dependencies too, so they are set before anything is declared.
if(HX_SANITIZE)
  add_compile_options(-fsanitize=${HX_SANITIZE} -fno-omit-frame-pointer -fno-sanitize-recover=all)
  add_link_options(-fsanitize=${HX_SANITIZE})
endif()

# --- Dependencies, each pinned to an exact revision ---------------------------------------------------------

include(FetchContent)

# Luau: the VM and the compiler. SYSTEM keeps its headers out of this project's warnings.
set(LUAU_BUILD_CLI OFF CACHE BOOL "" FORCE)
set(LUAU_BUILD_TESTS OFF CACHE BOOL "" FORCE)
FetchContent_Declare(
  luau
  GIT_REPOSITORY https://github.com/luau-lang/luau.git
  GIT_TAG 0.747
  GIT_SHALLOW TRUE
  EXCLUDE_FROM_ALL SYSTEM)

# RapidJSON: header-only streaming JSON reader and writer. Its last tagged release is from 2016 while master is
# maintained, so a master commit is pinned. SOURCE_SUBDIR names a directory that does not exist, which makes
# FetchContent download the sources without running the project's own CMake (tests, examples, docs).
FetchContent_Declare(
  rapidjson
  GIT_REPOSITORY https://github.com/Tencent/rapidjson.git
  GIT_TAG 297637bc0a0f00dc504dbc1a47ebdaa577f8ae19
  GIT_SUBMODULES ""
  SOURCE_SUBDIR not-configured)

# {fmt}: formatting and printing, used header-only.
FetchContent_Declare(
  fmt
  GIT_REPOSITORY https://github.com/fmtlib/fmt.git
  GIT_TAG 12.2.1
  GIT_SHALLOW TRUE
  EXCLUDE_FROM_ALL SYSTEM)

FetchContent_MakeAvailable(luau rapidjson fmt)

add_library(rapidjson INTERFACE)
target_include_directories(rapidjson SYSTEM INTERFACE ${rapidjson_SOURCE_DIR}/include)

find_package(CURL REQUIRED)

# --- hx ------------------------------------------------------------------------------------------------------

# The default loop is compiled into the binary, so hx works in a directory that has no .hx/ yet and still
# starts when a project's own loop is broken. The overlay, which `hx init` writes, is embedded the same way.
file(READ ${CMAKE_CURRENT_SOURCE_DIR}/lua/loop.luau HX_LOOP_SOURCE)
file(READ ${CMAKE_CURRENT_SOURCE_DIR}/lua/overlay.luau HX_OVERLAY_SOURCE)
configure_file(src/embedded.h.in embedded.h @ONLY)
set_property(DIRECTORY APPEND PROPERTY CMAKE_CONFIGURE_DEPENDS lua/loop.luau lua/overlay.luau)

add_executable(hx src/main.cpp src/host.cpp src/json.cpp src/sandbox.cpp)
target_include_directories(hx PRIVATE ${CMAKE_CURRENT_BINARY_DIR})
target_link_libraries(hx PRIVATE Luau.Compiler Luau.VM rapidjson fmt::fmt-header-only CURL::libcurl)
target_compile_options(
  hx
  PRIVATE -Wall
          -Wextra
          -Wpedantic
          -Wshadow
          -Wconversion
          -Wsign-conversion
          -Wold-style-cast
          -Wnon-virtual-dtor
          -Woverloaded-virtual
          -Wcast-align
          -Wformat=2
          -fstack-protector-strong
          $<$<BOOL:${HX_WERROR}>:-Werror>)
# Bounds checks in the standard library; cheap enough to keep in release builds.
target_compile_definitions(hx PRIVATE _GLIBCXX_ASSERTIONS)
target_link_options(hx PRIVATE -Wl,-z,relro,-z,now)

include(GNUInstallDirs)
install(TARGETS hx RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR})

# --- Tests ---------------------------------------------------------------------------------------------------

include(CTest)
if(BUILD_TESTING)
  add_test(NAME json COMMAND hx --run ${CMAKE_CURRENT_SOURCE_DIR}/tests/json_test.luau)
  add_test(NAME tools COMMAND hx --run ${CMAKE_CURRENT_SOURCE_DIR}/tests/tools_test.luau)
  add_test(NAME conversation COMMAND hx --run ${CMAKE_CURRENT_SOURCE_DIR}/tests/conversation_test.luau)
  add_test(NAME agent COMMAND python3 ${CMAKE_CURRENT_SOURCE_DIR}/tests/agent_test.py $<TARGET_FILE:hx>)
  set_tests_properties(json tools conversation PROPERTIES WORKING_DIRECTORY ${CMAKE_CURRENT_BINARY_DIR})
endif()
