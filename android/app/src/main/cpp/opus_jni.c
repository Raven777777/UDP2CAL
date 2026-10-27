#include <jni.h>
#include <opus.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <android/log.h>

#define TAG "OpusJNI"

/* 协议上限：单个 Opus 负载 1275 字节（对应 RTP 包 1287 字节） */
#define MAX_OPUS_PAYLOAD 1275

/* v3 固定 10 ms 帧：48 kHz 下为 480 采样/声道 */
#define FRAME_MS 10

typedef struct {
    OpusEncoder* encoder;
    int sample_rate;
    int frame_size;
    int bitrate_bps;
    int complexity;
    int signal_type;
    int bandwidth;
    int fec;
    int packet_loss;
    int vbr_constraint;
    int encode_count;
} EncoderState;

JNIEXPORT jlong JNICALL
Java_com_udp2cal_app_native_OpusNative_encoderCreate(
    JNIEnv* env, jclass clazz, jint sampleRate, jint bitrate,
    jint complexity, jint signalType, jint bandwidth,
    jint dtx, jint vbr, jint fec, jint packetLoss, jint vbrConstraint) {

    int err = 0;
    int frame_size = (sampleRate * FRAME_MS) / 1000;  // v3 固定 10 ms 帧（48 kHz → 480）

    OpusEncoder* enc = opus_encoder_create(sampleRate, 1, OPUS_APPLICATION_AUDIO, &err);
    if (err != OPUS_OK || enc == NULL) return 0;

    // BITRATE 必须放在 BANDWIDTH 之后，因为 OPUS_SET_BANDWIDTH 会重置内部码率
    opus_encoder_ctl(enc, OPUS_SET_VBR(vbr));
    opus_encoder_ctl(enc, OPUS_SET_DTX(dtx));
    opus_encoder_ctl(enc, OPUS_SET_INBAND_FEC(fec));
    opus_encoder_ctl(enc, OPUS_SET_PACKET_LOSS_PERC(packetLoss));
    // 防御：VBR=0 时 constraint 无意义，强制清零防止意外
    opus_encoder_ctl(enc, OPUS_SET_VBR_CONSTRAINT(vbr ? vbrConstraint : 0));
    // DRED 不在本项目中启用：接收端没有 DRED 解析/恢复路径，
    // 发送端单方面打开只会白占码率。
    opus_encoder_ctl(enc, OPUS_SET_DRED_DURATION(0));
    opus_encoder_ctl(enc, OPUS_SET_COMPLEXITY(complexity));
    opus_encoder_ctl(enc, OPUS_SET_SIGNAL(signalType));
    opus_encoder_ctl(enc, OPUS_SET_BANDWIDTH(bandwidth));
    opus_encoder_ctl(enc, OPUS_SET_BITRATE(bitrate * 1000));

    EncoderState* state = (EncoderState*)malloc(sizeof(EncoderState));
    if (state == NULL) { opus_encoder_destroy(enc); return 0; }
    state->encoder     = enc;
    state->sample_rate = sampleRate;
    state->frame_size  = frame_size;
    state->bitrate_bps = bitrate * 1000;
    state->complexity  = complexity;
    state->signal_type = signalType;
    state->bandwidth   = bandwidth;
    state->fec         = fec;
    state->packet_loss = packetLoss;
    state->vbr_constraint = vbrConstraint;
    state->encode_count = 0;

    __android_log_print(ANDROID_LOG_INFO, TAG,
        "Encoder: sr=%d br=%dk cplx=%d sig=%d bw=%d dtx=%d vbr=%d fec=%d pl=%d vbrc=%d",
        sampleRate, bitrate, complexity, signalType, bandwidth, dtx, vbr, fec, packetLoss, vbrConstraint);

    return (jlong)(uintptr_t)state;
}

JNIEXPORT jint JNICALL
Java_com_udp2cal_app_native_OpusNative_encoderEncodeTo(
    JNIEnv* env, jclass clazz, jlong handle, jshortArray pcmData,
    jbyteArray dest, jint offset) {

    if (handle == 0) return -1;
    EncoderState* state = (EncoderState*)(uintptr_t)handle;

    jsize len = (*env)->GetArrayLength(env, pcmData);
    if (len < state->frame_size) return -1;

    // ⚠ 双重边界守卫
    // 1) 静态阈值守卫：编码前确保剩余空间 ≥ 1275（协议规定的单帧负载上限）
    // 2) 动态精确守卫：编码后检查实际 nbBytes 是否越界
    jsize destLen = (*env)->GetArrayLength(env, dest);
    if (offset < 0 || offset >= destLen || (destLen - offset) < MAX_OPUS_PAYLOAD) {
        __android_log_print(ANDROID_LOG_ERROR, TAG,
            "Static overflow guard: destLen=%d offset=%d", destLen, offset);
        return -1;
    }

    jshort* pcm = (*env)->GetShortArrayElements(env, pcmData, NULL);
    if (pcm == NULL) return -1;

    // 输出缓冲严格取 1275 字节：opus_encode 会把码率钳制在该上限内，
    // 物理上不可能产出超过 RTP/Opus 允许的负载（超限宁可整帧丢弃也不截断）
    unsigned char packet[MAX_OPUS_PAYLOAD];
    int nbBytes = opus_encode(state->encoder, (const opus_int16*)pcm,
                               state->frame_size, packet, sizeof(packet));
    (*env)->ReleaseShortArrayElements(env, pcmData, pcm, JNI_ABORT);

    if (nbBytes < 0) return -1;

    // 动态精确守卫：编码后再次校验实际大小
    if (destLen - offset < (jsize)nbBytes) {
        __android_log_print(ANDROID_LOG_ERROR, TAG,
            "Dynamic overflow guard: destLen=%d offset=%d nbBytes=%d", destLen, offset, nbBytes);
        return -1;
    }

    state->encode_count++;
    if (state->encode_count % 100 == 0) {
        __android_log_print(ANDROID_LOG_DEBUG, TAG,
            "Audit: frame=%d in=%d out=%d ratio=%d%%",
            state->encode_count, state->frame_size * 2, nbBytes,
            (nbBytes * 100) / (state->frame_size * 2));
    }

    // 写入预分配的 dest 缓冲区，零分配
    jbyte* destBytes = (*env)->GetByteArrayElements(env, dest, NULL);
    if (destBytes == NULL) return -1;
    memcpy(destBytes + offset, packet, nbBytes);
    (*env)->ReleaseByteArrayElements(env, dest, destBytes, 0);

    return nbBytes;
}

JNIEXPORT jint JNICALL
Java_com_udp2cal_app_native_OpusNative_encoderGetFrameSize(
    JNIEnv* env, jclass clazz, jlong handle) {
    if (handle == 0) return 0;
    return ((EncoderState*)(uintptr_t)handle)->frame_size;
}

JNIEXPORT jboolean JNICALL
Java_com_udp2cal_app_native_OpusNative_encoderUpdate(
    JNIEnv* env, jclass clazz, jlong handle,
    jint complexity, jint signalType, jint bandwidth,
    jint dtx, jint vbr, jint bitrate,
    jint fec, jint packetLoss, jint vbrConstraint) {
    if (handle == 0) return JNI_FALSE;
    EncoderState* state = (EncoderState*)(uintptr_t)handle;
    // BITRATE 必须放在 BANDWIDTH 之后，因为 OPUS_SET_BANDWIDTH 会重置内部码率
    opus_encoder_ctl(state->encoder, OPUS_SET_COMPLEXITY(complexity));
    opus_encoder_ctl(state->encoder, OPUS_SET_SIGNAL(signalType));
    opus_encoder_ctl(state->encoder, OPUS_SET_BANDWIDTH(bandwidth));
    opus_encoder_ctl(state->encoder, OPUS_SET_DTX(dtx));
    opus_encoder_ctl(state->encoder, OPUS_SET_VBR(vbr));
    opus_encoder_ctl(state->encoder, OPUS_SET_INBAND_FEC(fec));
    opus_encoder_ctl(state->encoder, OPUS_SET_PACKET_LOSS_PERC(packetLoss));
    // DRED 始终关闭（接收端不支持解析/恢复）
    opus_encoder_ctl(state->encoder, OPUS_SET_DRED_DURATION(0));
    // 防御：VBR=0 时 constraint 无意义，强制清零防止意外
    opus_encoder_ctl(state->encoder, OPUS_SET_VBR_CONSTRAINT(vbr ? vbrConstraint : 0));
    opus_encoder_ctl(state->encoder, OPUS_SET_BITRATE(bitrate * 1000));
    state->complexity  = complexity;
    state->signal_type = signalType;
    state->bandwidth   = bandwidth;
    state->bitrate_bps = bitrate * 1000;
    state->fec         = fec;
    state->packet_loss = packetLoss;
    state->vbr_constraint = vbrConstraint;

    __android_log_print(ANDROID_LOG_DEBUG, TAG,
        "Update: cplx=%d sig=%d bw=%d dtx=%d vbr=%d br=%dk fec=%d pl=%d vbrc=%d",
        complexity, signalType, bandwidth, dtx, vbr, bitrate, fec, packetLoss, vbrConstraint);
    return JNI_TRUE;
}

JNIEXPORT void JNICALL
Java_com_udp2cal_app_native_OpusNative_encoderDestroy(
    JNIEnv* env, jclass clazz, jlong handle) {
    if (handle == 0) return;
    EncoderState* state = (EncoderState*)(uintptr_t)handle;
    if (state->encoder) { opus_encoder_destroy(state->encoder); state->encoder = NULL; }
    free(state);
}
