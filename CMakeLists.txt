cmake_minimum_required(VERSION 3.28)
project(
  hx
  VERSION 0.1.0
  DESCRIPTION "A small coding-agent host: a Luau VM, one tool that runs Luau, one sandbox"
  LANGUAGES CXX)

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
#
# A tag can be moved, so each is pinned by commit. A shallow clone cannot fetch a commit, so these are full clones.

include(FetchContent)

# Luau: the VM and the compiler. SYSTEM keeps its headers out of this project's warnings.
set(LUAU_BUILD_CLI OFF CACHE BOOL "" FORCE)
set(LUAU_BUILD_TESTS OFF CACHE BOOL "" FORCE)
FetchContent_Declare(
  luau
  GIT_REPOSITORY https://github.com/luau-lang/luau.git
  GIT_TAG c87565b8b0db5d5d5bfd2b8b00ad6573e5d06d9f # 0.747
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
  GIT_TAG 208219daff58f2a7438f6931cc2ddc358acf33ef # 12.2.1
  EXCLUDE_FROM_ALL SYSTEM)

FetchContent_MakeAvailable(luau rapidjson fmt)

add_library(rapidjson INTERFACE)
target_include_directories(rapidjson SYSTEM INTERFACE ${rapidjson_SOURCE_DIR}/include)

find_package(CURL REQUIRED)

# --- hx ------------------------------------------------------------------------------------------------------

# The default loop is compiled into the binary, so hx works in a directory that has no .hx/ yet and still
# starts when a project's own loop is broken. The overlay, which `hx --init` writes, the judge and the built-in
# extensions and skills are embedded the same way, each as a raw string literal in embedded.h.

# Reads a file into `var`, and reconfigures when it changes.
function(hx_embed var path)
  file(READ ${path} source)
  # The source goes between R"HXLUAU( and )HXLUAU"; the closing delimiter inside it would end the string early.
  string(FIND "${source}" ")HXLUAU\"" found)
  if(NOT found EQUAL -1)
    message(FATAL_ERROR "${path} contains )HXLUAU\", which would end its raw string in embedded.h early")
  endif()
  set_property(DIRECTORY APPEND PROPERTY CMAKE_CONFIGURE_DEPENDS ${path})
  set(${var} "${source}" PARENT_SCOPE)
endfunction()

hx_embed(HX_LOOP_SOURCE ${CMAKE_CURRENT_SOURCE_DIR}/lua/loop.luau)
hx_embed(HX_OVERLAY_SOURCE ${CMAKE_CURRENT_SOURCE_DIR}/lua/overlay.luau)
hx_embed(HX_JUDGE_SOURCE ${CMAKE_CURRENT_SOURCE_DIR}/lua/judge.luau)

# Every lua/ext/NAME.luau is the built-in extension NAME, so adding one is adding the file. CONFIGURE_DEPENDS
# reruns the glob at build time, so a new or removed file reconfigures too.
file(GLOB HX_EXT_FILES CONFIGURE_DEPENDS ${CMAKE_CURRENT_SOURCE_DIR}/lua/ext/*.luau)
list(SORT HX_EXT_FILES)
set(HX_EXTENSIONS "")
foreach(path IN LISTS HX_EXT_FILES)
  get_filename_component(name ${path} NAME_WLE)
  hx_embed(source ${path})
  string(APPEND HX_EXTENSIONS "    {\"${name}\", R\"HXLUAU(${source})HXLUAU\"},\n")
endforeach()

# The built-in skills, by name. Each is the SKILL.md of this repository's own skill of that name, in
# .claude/skills/, where Claude Code and hx both find it when they work here: one text for both, and for every
# project hx runs in. A skill is listed here, not globbed, because a skill for working on this repository alone
# does not belong in every project.
set(HX_SKILL_NAMES hx)
set(HX_SKILLS "")
foreach(name IN LISTS HX_SKILL_NAMES)
  hx_embed(source ${CMAKE_CURRENT_SOURCE_DIR}/.claude/skills/${name}/SKILL.md)
  string(APPEND HX_SKILLS "    {\"${name}\", R\"HXLUAU(${source})HXLUAU\"},\n")
endforeach()
configure_file(src/embedded.h.in embedded.h @ONLY)

add_executable(hx src/main.cpp src/host.cpp src/host_files.cpp src/host_http.cpp src/host_judge.cpp src/host_proc.cpp src/host_sh.cpp src/host_spawn.cpp src/judge.cpp src/json.cpp src/sandbox.cpp)
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
  # Each tests/NAME_test.luau is the test NAME. Other Luau files there, such as check.luau, are what the tests
  # share, and are not run on their own. A test runs in the build directory and reads check.luau from the source
  # directory, so that is opened to it with --ro.
  file(GLOB HX_LUAU_TESTS CONFIGURE_DEPENDS ${CMAKE_CURRENT_SOURCE_DIR}/tests/*_test.luau)
  list(SORT HX_LUAU_TESTS)
  foreach(test_file IN LISTS HX_LUAU_TESTS)
    get_filename_component(test_name ${test_file} NAME_WE)
    string(REGEX REPLACE "_test$" "" test_name ${test_name})
    add_test(NAME ${test_name} COMMAND hx -q --ro ${CMAKE_CURRENT_SOURCE_DIR}/tests --run ${test_file})
  endforeach()
  add_test(NAME agent COMMAND python3 ${CMAKE_CURRENT_SOURCE_DIR}/tests/agent_test.py $<TARGET_FILE:hx>)

  # The type checker and linter over every Luau file, when luau-analyze is installed. It runs in the source
  # directory, where .luaurc names the host's globals. It exits 0 when it finds only lint warnings, so any output
  # at all is a failure.
  find_program(LUAU_ANALYZE luau-analyze)
  if(LUAU_ANALYZE)
    file(GLOB HX_LUAU_FILES CONFIGURE_DEPENDS RELATIVE ${CMAKE_CURRENT_SOURCE_DIR}
         ${CMAKE_CURRENT_SOURCE_DIR}/lua/*.luau ${CMAKE_CURRENT_SOURCE_DIR}/lua/ext/*.luau
         ${CMAKE_CURRENT_SOURCE_DIR}/tests/*.luau)
    list(SORT HX_LUAU_FILES)
    add_test(NAME analyze COMMAND ${LUAU_ANALYZE} ${HX_LUAU_FILES} WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR})
    set_tests_properties(analyze PROPERTIES FAIL_REGULAR_EXPRESSION ".")
  endif()
endif()
