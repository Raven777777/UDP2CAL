package com.udp2cal.app.service

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update

/** 拆分后的各协程仍共用同一个 logcat tag，现场日志过滤方式保持不变 */
internal const val SERVICE_TAG = "CaptureService"

data class CaptureStatus(
    val isRunning: Boolean = false,
    val bitrateKbps: Float = 0f,
    val sampleRateHz: Int = 0,
    val bitrateTargetKbps: Int = 0,
    val vbrMode: String = "",
    val audioSource: String = "",
    val opusMode: String = "",
    val opusBandwidth: Int = 0,
    val errorMsg: String = "",
    val connected: Boolean = false,
    val deviceId: String = "",
    val reverseAudio: Boolean = false,
    val reverseBitrateKbps: Float = 0f,
    val reverseBw: String = "",
    /** 反向抖动缓冲摘要（仅非零项），每秒刷新，空串表示无数据或全为零 */
    val reverseJitter: String = "",
)

/**
 * 状态总线：正向 / 反向 / 控制协程都只写这里，不直接持有 Service。
 *
 * 统一走 [update] 做 CAS 更新，避免不同线程同时写不同字段时互相覆盖。
 */
internal class StatusBus {
    private val _status = MutableStateFlow(CaptureStatus())
    val flow: StateFlow<CaptureStatus> = _status.asStateFlow()
    val value: CaptureStatus get() = _status.value

    fun update(block: (CaptureStatus) -> CaptureStatus) {
        _status.update(block)
    }
}
