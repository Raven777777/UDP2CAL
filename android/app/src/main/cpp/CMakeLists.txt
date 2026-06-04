# CMakeLists.txt for UDP2Mic Android — Opus JNI encoder
cmake_minimum_required(VERSION 3.22.1)
project("udp2mic_opus")

# Build libopus 1.5.2 from local source
add_subdirectory(opus-src opus_build)

# JNI library: Opus encoder only
add_library(opus_jni SHARED opus_jni.c)

target_include_directories(opus_jni PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}/opus-src/include
)

target_compile_definitions(opus_jni PRIVATE HAVE_CONFIG_H=1)

target_link_libraries(opus_jni opus log)
