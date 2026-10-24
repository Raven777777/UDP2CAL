# CMakeLists.txt for UDP2CAL Android — Opus JNI
cmake_minimum_required(VERSION 3.22.1)
project("udp2cal_opus")

# 共享 libopus 源码树（仓库根目录，Android 与 Windows 共用同一份）
# 路径：<repo>/opus-src，相对本文件上溯 5 级
set(UDP2CAL_OPUS_SRC "${CMAKE_CURRENT_LIST_DIR}/../../../../../opus-src")
get_filename_component(UDP2CAL_OPUS_SRC "${UDP2CAL_OPUS_SRC}" ABSOLUTE)
if(NOT EXISTS "${UDP2CAL_OPUS_SRC}/CMakeLists.txt")
    message(FATAL_ERROR "未找到共享 libopus 源码树: ${UDP2CAL_OPUS_SRC}")
endif()

# Build libopus 1.6.1 from local source
# 注意：OPUS_OSCE / OPUS_DRED / QEXT 均保持默认关闭，本项目不使用其 ML 增强特性
add_subdirectory("${UDP2CAL_OPUS_SRC}" opus_build)

# JNI library: Opus encoder + decoder
add_library(opus_jni SHARED opus_jni.c opus_jni_decoder.c)

target_include_directories(opus_jni PRIVATE
    ${UDP2CAL_OPUS_SRC}/include
)

target_compile_definitions(opus_jni PRIVATE HAVE_CONFIG_H=1)

target_link_libraries(opus_jni opus log)
