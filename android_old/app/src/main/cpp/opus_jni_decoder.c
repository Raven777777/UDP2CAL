#include <jni.h>
#include <opus.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <android/log.h>

#define TAG "OpusDecodeJNI"

typedef struct {
    OpusDecoder* decoder;
    int sample_rate;
    int channels;
    int frame_size;  // 每通道采样数（20ms）
} DecoderState;

JNIEXPORT jlong JNICALL
Java_com_udp2cal_app_native_OpusDecodeNative_decoderCreate(
    JNIEnv* env, jclass clazz, jint sampleRate, jint channels) {

    int err = 0;
    int ch = (channels > 0) ? channels : 1;
    int frame_size = (sampleRate * 20) / 1000;

    OpusDecoder* dec = opus_decoder_create(sampleRate, ch, &err);
    if (err != OPUS_OK || dec == NULL) {
        __android_log_print(ANDROID_LOG_ERROR, TAG,
            "decoderCreate failed: err=%d sr=%d ch=%d", err, sampleRate, ch);
        return 0;
    }

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
    DecoderState* state = (DecoderState*)(uintptr_t)handle;

    jbyte* opus_bytes = (*env)->GetByteArrayElements(env, opusData, NULL);
    jshort* pcm = (*env)->GetShortArrayElements(env, pcmOut, NULL);

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

JNIEXPORT void JNICALL
Java_com_udp2cal_app_native_OpusDecodeNative_decoderDestroy(
    JNIEnv* env, jclass clazz, jlong handle) {
    if (handle == 0) return;
    DecoderState* state = (DecoderState*)(uintptr_t)handle;
    if (state->decoder) opus_decoder_destroy(state->decoder);
    free(state);
    __android_log_print(ANDROID_LOG_INFO, TAG, "Decoder destroyed");
}
