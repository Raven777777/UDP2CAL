#include <jni.h>
#include <opus.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <android/log.h>

#define TAG "OpusDecodeJNI"

/* v3 固定 10 ms 帧：48 kHz 下为 480 采样/声道。
 * 解码与 PLC 都必须按这个长度请求——一次请求 120 ms 再当一帧播放
 * 会让播放头与媒体时间线彻底错位。 */
#define FRAME_MS 10

typedef struct {
    OpusDecoder* decoder;
    int sample_rate;
    int channels;
    int frame_size;  // 每通道采样数（10 ms = 480）
} DecoderState;

JNIEXPORT jlong JNICALL
Java_com_udp2cal_app_native_OpusDecodeNative_decoderCreate(
    JNIEnv* env, jclass clazz, jint sampleRate, jint channels) {

    int err = 0;
    int ch = (channels > 0) ? channels : 1;
    int frame_size = (sampleRate * FRAME_MS) / 1000;

    OpusDecoder* dec = opus_decoder_create(sampleRate, ch, &err);
    if (err != OPUS_OK || dec == NULL) {
        __android_log_print(ANDROID_LOG_ERROR, TAG,
            "decoderCreate failed: err=%d sr=%d ch=%d", err, sampleRate, ch);
        return 0;
    }

    // Opus 1.5+：解码复杂度 >=5 启用 Deep PLC（库需 OPUS_DEEP_PLC 构建，见 CMakeLists）
    opus_decoder_ctl(dec, OPUS_SET_COMPLEXITY(5));

    DecoderState* state = (DecoderState*)malloc(sizeof(DecoderState));
    if (state == NULL) { opus_decoder_destroy(dec); return 0; }
    state->decoder     = dec;
    state->sample_rate = sampleRate;
    state->channels    = ch;
    state->frame_size  = frame_size;

    __android_log_print(ANDROID_LOG_INFO, TAG,
        "Decoder created: sr=%d ch=%d frame=%d", sampleRate, ch, frame_size);

    return (jlong)(uintptr_t)state;
}

JNIEXPORT jint JNICALL
Java_com_udp2cal_app_native_OpusDecodeNative_decoderDecode(
    JNIEnv* env, jclass clazz, jlong handle,
    jbyteArray opusData, jint offset, jint length,
    jshortArray pcmOut, jint pcmOffset) {

    if (handle == 0) return -1;
    if (opusData == NULL || pcmOut == NULL) return -1;
    DecoderState* state = (DecoderState*)(uintptr_t)handle;

    // 入参边界校验：越界的 offset/length 会在 JNI 层直接造成越界访问
    jsize data_len = (*env)->GetArrayLength(env, opusData);
    jsize pcm_len  = (*env)->GetArrayLength(env, pcmOut);
    if (offset < 0 || length <= 0 || offset > data_len - length) {
        __android_log_print(ANDROID_LOG_ERROR, TAG,
            "decode: bad range offset=%d len=%d size=%d", offset, length, data_len);
        return -1;
    }
    if (pcmOffset < 0 || pcmOffset > pcm_len - state->frame_size * state->channels) {
        __android_log_print(ANDROID_LOG_ERROR, TAG,
            "decode: pcm buffer too small off=%d need=%d size=%d",
            pcmOffset, state->frame_size * state->channels, pcm_len);
        return -1;
    }

    // Get*ArrayElements 可能因 OOM 返回 NULL，未判空会直接空指针解引用
    jbyte* opus_bytes = (*env)->GetByteArrayElements(env, opusData, NULL);
    if (opus_bytes == NULL) return -1;
    jshort* pcm = (*env)->GetShortArrayElements(env, pcmOut, NULL);
    if (pcm == NULL) {
        (*env)->ReleaseByteArrayElements(env, opusData, opus_bytes, JNI_ABORT);
        return -1;
    }

    int nb_samples = opus_decode(
        state->decoder,
        (const unsigned char*)(opus_bytes + offset),
        length,
        (opus_int16*)(pcm + pcmOffset),
        state->frame_size,
        0  // FEC off
    );

    (*env)->ReleaseShortArrayElements(env, pcmOut, pcm, 0);
    (*env)->ReleaseByteArrayElements(env, opusData, opus_bytes, JNI_ABORT);

    if (nb_samples < 0) {
        __android_log_print(ANDROID_LOG_WARN, TAG, "decode error: %d", nb_samples);
        return -1;
    }
    // 返回实际解码的总采样数（nb_samples × channels）
    return nb_samples * state->channels;
}

/* 用下一个包里的 LBRR 冗余重建刚刚丢失的那一帧（Opus 带内 FEC）。
 *
 * opus_decode(..., decode_fec = 1) 不解析这个包自己的音频，而是用它携带的
 * 冗余还原**上一帧**；因此调用方必须保证同一个包随后还会用 fec = 0 正常解码
 * 一次，解码器状态才与发送端连续。包里没有 LBRR（对端没开带内 FEC）时
 * libopus 自动退化成 PLC，所以这条路径不会比 decoderPlc 更差。
 *
 * 注意 frame_size 参数给的是 state->frame_size（480/声道）：opus_decode 在
 * FEC 模式下会把调用方给的输出长度填满，多出来的帧会被上层当成"这一帧"
 * 播放，所以这里绝不能给出大于 10 ms 的容量。 */
JNIEXPORT jint JNICALL
Java_com_udp2cal_app_native_OpusDecodeNative_decoderDecodeFec(
    JNIEnv* env, jclass clazz, jlong handle,
    jbyteArray opusData, jint offset, jint length,
    jshortArray pcmOut, jint pcmOffset) {

    if (handle == 0) return -1;
    if (opusData == NULL || pcmOut == NULL) return -1;
    DecoderState* state = (DecoderState*)(uintptr_t)handle;

    jsize data_len = (*env)->GetArrayLength(env, opusData);
    jsize pcm_len  = (*env)->GetArrayLength(env, pcmOut);
    if (offset < 0 || length <= 0 || offset > data_len - length) return -1;
    if (pcmOffset < 0 || pcmOffset > pcm_len - state->frame_size * state->channels) {
        return -1;
    }

    jbyte* opus_bytes = (*env)->GetByteArrayElements(env, opusData, NULL);
    if (opus_bytes == NULL) return -1;
    jshort* pcm = (*env)->GetShortArrayElements(env, pcmOut, NULL);
    if (pcm == NULL) {
        (*env)->ReleaseByteArrayElements(env, opusData, opus_bytes, JNI_ABORT);
        return -1;
    }

    int nb_samples = opus_decode(
        state->decoder,
        (const unsigned char*)(opus_bytes + offset),
        length,
        (opus_int16*)(pcm + pcmOffset),
        state->frame_size,
        1  // FEC on
    );

    (*env)->ReleaseShortArrayElements(env, pcmOut, pcm, 0);
    (*env)->ReleaseByteArrayElements(env, opusData, opus_bytes, JNI_ABORT);

    /* 时间线固定 10 ms：只接受恰好一帧，其它长度按失败处理让上层退回 PLC */
    if (nb_samples != state->frame_size) {
        if (nb_samples >= 0) {
            __android_log_print(ANDROID_LOG_WARN, TAG,
                "fec decode returned %d samples, expected %d", nb_samples, state->frame_size);
        }
        return -1;
    }
    return nb_samples * state->channels;
}

JNIEXPORT jint JNICALL
Java_com_udp2cal_app_native_OpusDecodeNative_decoderPlc(
    JNIEnv* env, jclass clazz, jlong handle,
    jshortArray pcmOut, jint pcmOffset) {

    if (handle == 0 || pcmOut == NULL) return -1;
    DecoderState* state = (DecoderState*)(uintptr_t)handle;

    jsize pcm_len = (*env)->GetArrayLength(env, pcmOut);
    if (pcmOffset < 0 || pcmOffset > pcm_len - state->frame_size * state->channels) {
        __android_log_print(ANDROID_LOG_ERROR, TAG,
            "plc: pcm buffer too small off=%d need=%d size=%d",
            pcmOffset, state->frame_size * state->channels, pcm_len);
        return -1;
    }

    jshort* pcm = (*env)->GetShortArrayElements(env, pcmOut, NULL);
    if (pcm == NULL) return -1;

    // data=NULL + length=0 → 丢包隐藏（PLC）。
    // 库以 OPUS_DEEP_PLC 构建且解码复杂度 >=5 时为神经网络补偿，否则经典 PLC。
    int nb_samples = opus_decode(
        state->decoder, NULL, 0,
        (opus_int16*)(pcm + pcmOffset),
        state->frame_size, 0
    );

    (*env)->ReleaseShortArrayElements(env, pcmOut, pcm, 0);

    if (nb_samples < 0) {
        __android_log_print(ANDROID_LOG_WARN, TAG, "plc error: %d", nb_samples);
        return -1;
    }
    return nb_samples * state->channels;
}

JNIEXPORT void JNICALL
Java_com_udp2cal_app_native_OpusDecodeNative_decoderDestroy(
    JNIEnv* env, jclass clazz, jlong handle) {
    if (handle == 0) return;
    DecoderState* state = (DecoderState*)(uintptr_t)handle;
    if (state->decoder) opus_decoder_destroy(state->decoder);
    free(state);
    __android_log_print(ANDROID_LOG_INFO, TAG, "Decoder destroyed");
}
