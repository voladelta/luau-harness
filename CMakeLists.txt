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
set(CMAKE_CXX_SCAN_FOR_MODULES OFF)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
  set(CMAKE_BUILD_TYPE Release CACHE STRING "Build type" FORCE)
endif()

include(CheckPIESupported)
check_pie_supported()

if(HX_SANITIZE)
  add_compile_options(-fsanitize=${HX_SANITIZE} -fno-omit-frame-pointer -fno-sanitize-recover=all)
  add_link_options(-fsanitize=${HX_SANITIZE})
endif()

include(FetchContent)

set(LUAU_BUILD_CLI OFF CACHE BOOL "" FORCE)
set(LUAU_BUILD_TESTS OFF CACHE BOOL "" FORCE)
FetchContent_Declare(
  luau
  GIT_REPOSITORY https://github.com/luau-lang/luau.git
  GIT_TAG c87565b8b0db5d5d5bfd2b8b00ad6573e5d06d9f
  EXCLUDE_FROM_ALL SYSTEM)

set(YYJSON_READER_DEPTH_LIMIT 200 CACHE STRING "" FORCE)
set(YYJSON_INSTALL OFF CACHE BOOL "" FORCE)
set(YYJSON_DISABLE_INCR_READER ON CACHE BOOL "" FORCE)
set(YYJSON_DISABLE_FILE ON CACHE BOOL "" FORCE)
set(YYJSON_DISABLE_UTILS ON CACHE BOOL "" FORCE)
FetchContent_Declare(
  yyjson
  GIT_REPOSITORY https://github.com/ibireme/yyjson.git
  GIT_TAG 6447536015f3d600f3d65323b10976103b337ca7
  EXCLUDE_FROM_ALL SYSTEM)

FetchContent_Declare(
  fmt
  GIT_REPOSITORY https://github.com/fmtlib/fmt.git
  GIT_TAG 208219daff58f2a7438f6931cc2ddc358acf33ef
  EXCLUDE_FROM_ALL SYSTEM)

FetchContent_MakeAvailable(luau yyjson fmt)

find_package(CURL REQUIRED)

function(hx_embed var path)
  file(READ ${path} source)
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

file(GLOB HX_EXT_FILES CONFIGURE_DEPENDS ${CMAKE_CURRENT_SOURCE_DIR}/lua/ext/*.luau)
list(SORT HX_EXT_FILES)
set(HX_EXTENSIONS "")
foreach(path IN LISTS HX_EXT_FILES)
  get_filename_component(name ${path} NAME_WLE)
  hx_embed(source ${path})
  string(APPEND HX_EXTENSIONS "    {\"${name}\", R\"HXLUAU(${source})HXLUAU\"},\n")
endforeach()

set(HX_SKILL_NAMES hx)
set(HX_SKILLS "")
foreach(name IN LISTS HX_SKILL_NAMES)
  hx_embed(source ${CMAKE_CURRENT_SOURCE_DIR}/.claude/skills/${name}/SKILL.md)
  string(APPEND HX_SKILLS "    {\"${name}\", R\"HXLUAU(${source})HXLUAU\"},\n")
endforeach()
file(GLOB HX_SKILL_FILES CONFIGURE_DEPENDS ${CMAKE_CURRENT_SOURCE_DIR}/lua/skills/*/SKILL.md)
list(SORT HX_SKILL_FILES)
foreach(path IN LISTS HX_SKILL_FILES)
  get_filename_component(dir ${path} DIRECTORY)
  get_filename_component(name ${dir} NAME)
  hx_embed(source ${path})
  string(APPEND HX_SKILLS "    {\"${name}\", R\"HXLUAU(${source})HXLUAU\"},\n")
endforeach()
configure_file(src/embedded.h.in embedded.h @ONLY)

add_executable(hx src/main.cpp src/cli.cpp src/policy.cpp src/vm.cpp src/host.cpp src/host_files.cpp src/host_http.cpp src/host_judge.cpp src/host_proc.cpp src/host_sh.cpp src/host_spawn.cpp src/judge.cpp src/json.cpp src/sandbox.cpp)
target_include_directories(hx PRIVATE ${CMAKE_CURRENT_BINARY_DIR})
target_link_libraries(hx PRIVATE Luau.Compiler Luau.VM yyjson fmt::fmt-header-only CURL::libcurl)
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
target_compile_definitions(hx PRIVATE _GLIBCXX_ASSERTIONS)
target_link_options(hx PRIVATE -Wl,-z,relro,-z,now)

include(GNUInstallDirs)
install(TARGETS hx RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR})

include(CTest)
if(BUILD_TESTING)
  file(GLOB HX_LUAU_TESTS CONFIGURE_DEPENDS ${CMAKE_CURRENT_SOURCE_DIR}/tests/*_test.luau)
  list(SORT HX_LUAU_TESTS)
  foreach(test_file IN LISTS HX_LUAU_TESTS)
    get_filename_component(test_name ${test_file} NAME_WE)
    string(REGEX REPLACE "_test$" "" test_name ${test_name})
    set(test_dir ${CMAKE_CURRENT_BINARY_DIR}/test-projects/${test_name})
    file(MAKE_DIRECTORY ${test_dir})
    add_test(NAME ${test_name} COMMAND hx -q --ro ${CMAKE_CURRENT_SOURCE_DIR}/tests --run ${test_file}
             WORKING_DIRECTORY ${test_dir})
  endforeach()
  add_test(NAME agent COMMAND python3 ${CMAKE_CURRENT_SOURCE_DIR}/tests/agent_test.py $<TARGET_FILE:hx>)

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
