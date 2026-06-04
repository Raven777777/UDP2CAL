#include <jni.h>
#include <opus.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <android/log.h>

#define TAG "OpusJNI"

typedef struct {
    OpusEncoder* encoder;
    int sample_rate;
    int frame_size;
    int bitrate_bps;
    int complexity;
    int signal_type;
    int bandwidth;
    int encode_count;
} EncoderState;

JNIEXPORT jlong JNICALL
Java_com_udp2mic_app_native_OpusNative_encoderCreate(
    JNIEnv* env, jclass clazz, jint sampleRate, jint bitrate,
    jint complexity, jint signalType, jint bandwidth,
    jint dtx, jint vbr) {

    int err = 0;
    int frame_size = (sampleRate * 20) / 1000;

    OpusEncoder* enc = opus_encoder_create(sampleRate, 1, OPUS_APPLICATION_AUDIO, &err);
    if (err != OPUS_OK || enc == NULL) return 0;

    opus_encoder_ctl(enc, OPUS_SET_BITRATE(bitrate * 1000));
    opus_encoder_ctl(enc, OPUS_SET_VBR(vbr));
    opus_encoder_ctl(enc, OPUS_SET_DTX(dtx));
    opus_encoder_ctl(enc, OPUS_SET_INBAND_FEC(0));
    opus_encoder_ctl(enc, OPUS_SET_COMPLEXITY(complexity));
    opus_encoder_ctl(enc, OPUS_SET_SIGNAL(signalType));
    opus_encoder_ctl(enc, OPUS_SET_BANDWIDTH(bandwidth));

    EncoderState* state = (EncoderState*)malloc(sizeof(EncoderState));
    if (state == NULL) { opus_encoder_destroy(enc); return 0; }
    state->encoder     = enc;
    state->sample_rate = sampleRate;
    state->frame_size  = frame_size;
    state->bitrate_bps = bitrate * 1000;
    state->complexity  = complexity;
    state->signal_type = signalType;
    state->bandwidth   = bandwidth;
    state->encode_count = 0;

    __android_log_print(ANDROID_LOG_INFO, TAG,
        "Encoder: sr=%d br=%dk cplx=%d sig=%d bw=%d dtx=%d vbr=%d",
        sampleRate, bitrate, complexity, signalType, bandwidth, dtx, vbr);

    return (jlong)(intptr_t)state;
}

JNIEXPORT jbyteArray JNICALL
Java_com_udp2mic_app_native_OpusNative_encoderEncode(
    JNIEnv* env, jclass clazz, jlong handle, jshortArray pcmData) {

    if (handle == 0) return NULL;
    EncoderState* state = (EncoderState*)(intptr_t)handle;

    jsize len = (*env)->GetArrayLength(env, pcmData);
    if (len < state->frame_size) return NULL;

    jshort* pcm = (*env)->GetShortArrayElements(env, pcmData, NULL);
    if (pcm == NULL) return NULL;

    unsigned char packet[4096];
    int nbBytes = opus_encode(state->encoder, (const opus_int16*)pcm,
                               state->frame_size, packet, sizeof(packet));
    (*env)->ReleaseShortArrayElements(env, pcmData, pcm, JNI_ABORT);

    if (nbBytes < 0) return NULL;

    state->encode_count++;
    if (state->encode_count % 50 == 0) {
        __android_log_print(ANDROID_LOG_DEBUG, TAG,
            "Audit: frame=%d in=%d out=%d ratio=%d%%",
            state->encode_count, state->frame_size * 2, nbBytes,
            (nbBytes * 100) / (state->frame_size * 2));
    }

    jbyteArray result = (*env)->NewByteArray(env, nbBytes);
    if (result != NULL)
        (*env)->SetByteArrayRegion(env, result, 0, nbBytes, (const jbyte*)packet);
    return result;
}

JNIEXPORT jint JNICALL
Java_com_udp2mic_app_native_OpusNative_encoderGetFrameSize(
    JNIEnv* env, jclass clazz, jlong handle) {
    if (handle == 0) return 0;
    return ((EncoderState*)(intptr_t)handle)->frame_size;
}

JNIEXPORT jboolean JNICALL
Java_com_udp2mic_app_native_OpusNative_encoderUpdate(
    JNIEnv* env, jclass clazz, jlong handle,
    jint complexity, jint signalType, jint bandwidth,
    jint dtx, jint vbr, jint bitrate) {
    if (handle == 0) return JNI_FALSE;
    EncoderState* state = (EncoderState*)(intptr_t)handle;
    opus_encoder_ctl(state->encoder, OPUS_SET_COMPLEXITY(complexity));
    opus_encoder_ctl(state->encoder, OPUS_SET_SIGNAL(signalType));
    opus_encoder_ctl(state->encoder, OPUS_SET_BANDWIDTH(bandwidth));
    opus_encoder_ctl(state->encoder, OPUS_SET_DTX(dtx));
    opus_encoder_ctl(state->encoder, OPUS_SET_VBR(vbr));
    opus_encoder_ctl(state->encoder, OPUS_SET_BITRATE(bitrate * 1000));
    state->complexity = complexity;
    state->signal_type = signalType;
    state->bandwidth = bandwidth;
    state->bitrate_bps = bitrate * 1000;

    __android_log_print(ANDROID_LOG_DEBUG, TAG,
        "Update: cplx=%d sig=%d bw=%d dtx=%d vbr=%d br=%dk",
        complexity, signalType, bandwidth, dtx, vbr, bitrate);
    return JNI_TRUE;
}

JNIEXPORT void JNICALL
Java_com_udp2mic_app_native_OpusNative_encoderDestroy(
    JNIEnv* env, jclass clazz, jlong handle) {
    if (handle == 0) return;
    EncoderState* state = (EncoderState*)(intptr_t)handle;
    if (state->encoder) { opus_encoder_destroy(state->encoder); state->encoder = NULL; }
    free(state);
}
