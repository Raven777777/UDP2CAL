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
    int frame_size;
} DecoderState;

JNIEXPORT jlong JNICALL
Java_com_udp2cal_app_native_OpusDecodeNative_decoderCreate(
    JNIEnv* env, jclass clazz, jint sampleRate) {

    int err = 0;
    int frame_size = (sampleRate * 20) / 1000;

    OpusDecoder* dec = opus_decoder_create(sampleRate, 1, &err);
    if (err != OPUS_OK || dec == NULL) {
        __android_log_print(ANDROID_LOG_ERROR, TAG,
            "decoderCreate failed: err=%d sr=%d", err, sampleRate);
        return 0;
    }

    DecoderState* state = (DecoderState*)malloc(sizeof(DecoderState));
    if (state == NULL) { opus_decoder_destroy(dec); return 0; }
    state->decoder     = dec;
    state->sample_rate = sampleRate;
    state->frame_size  = frame_size;

    __android_log_print(ANDROID_LOG_INFO, TAG,
        "Decoder created: sr=%d frame=%d", sampleRate, frame_size);

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
    return nb_samples;
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
