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
    int encode_count;
} EncoderState;

JNIEXPORT jlong JNICALL
Java_com_udp2mic_app_native_OpusNative_encoderCreate(
    JNIEnv* env, jclass clazz, jint sampleRate, jint bitrate) {

    int err = 0;
    int frame_size = (sampleRate * 20) / 1000;

    OpusEncoder* enc = opus_encoder_create(sampleRate, 1, OPUS_APPLICATION_AUDIO, &err);
    if (err != OPUS_OK || enc == NULL) {
        return 0;
    }

    opus_encoder_ctl(enc, OPUS_SET_BITRATE(bitrate * 1000));
    opus_encoder_ctl(enc, OPUS_SET_VBR(0));
    opus_encoder_ctl(enc, OPUS_SET_COMPLEXITY(10));
    opus_encoder_ctl(enc, OPUS_SET_DTX(0));
    opus_encoder_ctl(enc, OPUS_SET_INBAND_FEC(0));
    opus_encoder_ctl(enc, OPUS_SET_SIGNAL(OPUS_SIGNAL_MUSIC));
    opus_encoder_ctl(enc, OPUS_SET_BANDWIDTH(OPUS_BANDWIDTH_FULLBAND));

    EncoderState* state = (EncoderState*)malloc(sizeof(EncoderState));
    if (state == NULL) {
        opus_encoder_destroy(enc);
        return 0;
    }
    state->encoder = enc;
    state->sample_rate = sampleRate;
    state->frame_size = frame_size;
    state->bitrate_bps = bitrate * 1000;
    state->encode_count = 0;
    return (jlong)(intptr_t)state;
}

JNIEXPORT jstring JNICALL
Java_com_udp2mic_app_native_OpusNative_encoderGetDebugInfo(
    JNIEnv* env, jclass clazz, jlong handle) {

    if (handle == 0) return NULL;
    EncoderState* state = (EncoderState*)(intptr_t)handle;

    opus_int32 bitrate = 0, vbr = 0, complexity = 0, signal = 0, dtx = 0;
    opus_encoder_ctl(state->encoder, OPUS_GET_BITRATE(&bitrate));
    opus_encoder_ctl(state->encoder, OPUS_GET_VBR(&vbr));
    opus_encoder_ctl(state->encoder, OPUS_GET_COMPLEXITY(&complexity));
    opus_encoder_ctl(state->encoder, OPUS_GET_SIGNAL(&signal));
    opus_encoder_ctl(state->encoder, OPUS_GET_DTX(&dtx));

    const char* sig_str = "AUTO";
    if (signal == OPUS_SIGNAL_VOICE) sig_str = "VOICE";
    else if (signal == OPUS_SIGNAL_MUSIC) sig_str = "MUSIC";

    char buf[512];
    int n = snprintf(buf, sizeof(buf),
        "{"
        "\"sample_rate\":%d,"
        "\"frame_size\":%d,"
        "\"bitrate_bps\":%d,"
        "\"actual_bitrate_bps\":%d,"
        "\"vbr\":%d,"
        "\"complexity\":%d,"
        "\"signal\":\"%s\","
        "\"dtx\":%d"
        "}",
        state->sample_rate, state->frame_size, state->bitrate_bps,
        (int)bitrate, (int)vbr, (int)complexity, sig_str, (int)dtx);
    if (n < 0) return NULL;

    return (*env)->NewStringUTF(env, buf);
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

    // ★ 审计: 每50帧记录编码输出大小
    state->encode_count++;
    if (state->encode_count % 50 == 0) {
        __android_log_print(ANDROID_LOG_DEBUG, TAG,
            "编码审计: frame=%d frameSize=%d inBytes=%d outBytes=%d ratio=%d%%",
            state->encode_count, state->frame_size,
            state->frame_size * 2, nbBytes,
            (nbBytes * 100) / (state->frame_size * 2));
    }

    jbyteArray result = (*env)->NewByteArray(env, nbBytes);
    if (result != NULL) {
        (*env)->SetByteArrayRegion(env, result, 0, nbBytes, (const jbyte*)packet);
    }
    return result;
}

JNIEXPORT jint JNICALL
Java_com_udp2mic_app_native_OpusNative_encoderGetFrameSize(
    JNIEnv* env, jclass clazz, jlong handle) {

    if (handle == 0) return 0;
    EncoderState* state = (EncoderState*)(intptr_t)handle;
    return state->frame_size;
}

JNIEXPORT void JNICALL
Java_com_udp2mic_app_native_OpusNative_encoderDestroy(
    JNIEnv* env, jclass clazz, jlong handle) {

    if (handle == 0) return;
    EncoderState* state = (EncoderState*)(intptr_t)handle;
    if (state->encoder != NULL) {
        opus_encoder_destroy(state->encoder);
        state->encoder = NULL;
    }
    free(state);
}
