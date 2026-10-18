# Builds the vendored jpegli encoder (and Highway, its SIMD dispatch) into static libraries, plus the C shim the
# Rust side calls. Only what encoding needs: no CMS, tools, tests or libjpeg.so replacement.
cmake_minimum_required(VERSION 3.16)
project(wirephoto_jpegli C CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
set(BUILD_SHARED_LIBS OFF CACHE BOOL "" FORCE)

set(HWY_FORCE_STATIC_LIBS ON CACHE INTERNAL "")
set(HWY_ENABLE_CONTRIB OFF CACHE INTERNAL "")
set(HWY_ENABLE_EXAMPLES OFF CACHE INTERNAL "")
set(HWY_ENABLE_INSTALL OFF CACHE INTERNAL "")
set(HWY_ENABLE_TESTS OFF CACHE INTERNAL "")
add_subdirectory(vendor/highway EXCLUDE_FROM_ALL)

set(J ${CMAKE_CURRENT_SOURCE_DIR}/vendor/jpegli)
set(JPEG_LIB_VERSION 62)
set(BITS_IN_JSAMPLE 8)
set(MEM_SRCDST_SUPPORTED 1)
set(VERSION 3.1.0)
set(LIBJPEG_TURBO_VERSION_NUMBER 3001000)
set(GEN ${CMAKE_CURRENT_BINARY_DIR}/include)
configure_file(${J}/third_party/libjpeg-turbo/jconfig.h.in ${GEN}/jconfig.h)
configure_file(${J}/third_party/libjpeg-turbo/jpeglib.h ${GEN}/jpeglib.h COPYONLY)
configure_file(${J}/third_party/libjpeg-turbo/jmorecfg.h ${GEN}/jmorecfg.h COPYONLY)

include(${J}/lib/jpegli_lists.cmake)
list(TRANSFORM JPEGLI_INTERNAL_JPEGLI_SOURCES PREPEND ${J}/lib/)
add_library(jpegli STATIC ${JPEGLI_INTERNAL_JPEGLI_SOURCES} shim/shim.cc)
target_include_directories(jpegli BEFORE PRIVATE ${GEN} ${J})
target_link_libraries(jpegli PRIVATE hwy)
target_compile_options(jpegli PRIVATE -fno-exceptions -fmerge-all-constants -Wno-unused-parameter)

install(TARGETS jpegli ARCHIVE DESTINATION lib)
install(FILES $<TARGET_FILE:hwy> DESTINATION lib)
