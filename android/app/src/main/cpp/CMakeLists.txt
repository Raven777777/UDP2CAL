# CMakeLists.txt for UDP2CAL Android — Opus JNI
cmake_minimum_required(VERSION 3.22.1)
project("udp2cal_opus")

# Build libopus 1.5.2 from local source
add_subdirectory(opus-src opus_build)

# JNI library: Opus encoder + decoder
add_library(opus_jni SHARED opus_jni.c opus_jni_decoder.c)

target_include_directories(opus_jni PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}/opus-src/include
)

target_compile_definitions(opus_jni PRIVATE HAVE_CONFIG_H=1)

target_link_libraries(opus_jni opus log)
