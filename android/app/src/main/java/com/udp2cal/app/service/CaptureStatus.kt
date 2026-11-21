package com.udp2cal.app.service

import androidx.annotation.StringRes
import com.udp2cal.app.R
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update

/** 拆分后的各协程仍共用同一个 logcat tag，现场日志过滤方式保持不变 */
internal const val SERVICE_TAG = "CaptureService"

/** 正向编码模式：语音优化 / 全频。界面和通知按此值选文案。 */
enum class CaptureMode { VOICE, FULLBAND }

/** 实际生效的麦克风音源。 */
enum class AudioSource {
    MIC_DIRECT,
    MIC_DIRECT_FALLBACK,
    RAW_UNPROCESSED,
    VOICE_RECOGNITION,
    SYSTEM_NS,
    SYSTEM_NS_FALLBACK,
    TEST_TONE,
}

enum class AecState { NONE, ENABLED, UNAVAILABLE }

enum class VbrMode { CBR, VBR, VBR_CONSTRAINED }

@get:StringRes
internal val CaptureMode.labelRes: Int
    get() = when (this) {
        CaptureMode.VOICE -> R.string.mode_voice
        CaptureMode.FULLBAND -> R.string.mode_fullband
    }

@get:StringRes
internal val AudioSource.sourceRes: Int
    get() = when (this) {
        AudioSource.MIC_DIRECT -> R.string.source_mic_direct
        AudioSource.MIC_DIRECT_FALLBACK -> R.string.source_mic_direct_fallback
        AudioSource.RAW_UNPROCESSED -> R.string.source_raw_unprocessed
        AudioSource.VOICE_RECOGNITION -> R.string.source_voice_recognition
        AudioSource.SYSTEM_NS -> R.string.source_system_ns
        AudioSource.SYSTEM_NS_FALLBACK -> R.string.source_system_ns_fallback
        AudioSource.TEST_TONE -> R.string.source_test_tone
    }

@get:StringRes
internal val AecState.aecRes: Int
    get() = when (this) {
        AecState.NONE -> 0
        AecState.ENABLED -> R.string.source_aec_enabled
        AecState.UNAVAILABLE -> R.string.source_aec_unavailable
    }

@get:StringRes
internal val VbrMode.vbrRes: Int
    get() = when (this) {
        VbrMode.CBR -> R.string.vbr_cbr
        VbrMode.VBR -> R.string.vbr_vbr
        VbrMode.VBR_CONSTRAINED -> R.string.vbr_constrained
    }

data class CaptureStatus(
    val isRunning: Boolean = false,
    val bitrateKbps: Float = 0f,
    val sampleRateHz: Int = 0,
    val bitrateTargetKbps: Int = 0,
    val vbrMode: VbrMode = VbrMode.CBR,
    val audioSource: AudioSource = AudioSource.MIC_DIRECT,
    val aec: AecState = AecState.NONE,
    val mode: CaptureMode = CaptureMode.FULLBAND,
    val opusBandwidth: Int = 0,
    /** 资源化错误：切换语言后可由界面按当前 locale 重新取词 */
    @StringRes val errorRes: Int = 0,
    val errorArgs: List<Any> = emptyList(),
    /** 硬件初始化硬故障：禁止自动重启（文案可能被翻译，不能用字符串判断） */
    val fatal: Boolean = false,
    val connected: Boolean = false,
    val deviceId: String = "",
    val reverseAudio: Boolean = false,
    val reverseBitrateKbps: Float = 0f,
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
