package com.udp2cal.app

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.util.Log
import java.net.DatagramSocket

/**
 * Wi-Fi 网络选择与 socket 绑定（P1）。
 *
 * 手机同时有蜂窝/VPN 时，未绑定的 UDP socket 走系统默认路由：Wi-Fi 一断，
 * 发往局域网 IP 的包会被外壳网络接走，表现为"莫名其妙断开"，网络切换时
 * 也可能短暂走错接口。这里统一选出 TRANSPORT_WIFI 的 [Network]，让正向、
 * 反向 socket 和发现广播都钉在同一张 Wi-Fi 上。
 *
 * [start] 注册的 [ConnectivityManager.NetworkCallback] 只负责通知上层
 * "Wi-Fi 来了/走了"；选择网络时仍以同步查询为准，避免回调尚未送达时
 * 拿到过期的缓存。
 */
object WifiNetworkBinder {
    private const val TAG = "WifiNetworkBinder"

    interface Listener {
        fun onWifiAvailable(network: Network)
        fun onWifiLost(network: Network)
    }

    private val lock = Any()

    @Volatile
    private var connectivity: ConnectivityManager? = null

    /** 回调上报过的在线 Wi-Fi 网络；元素集合整体替换，跨线程读取安全 */
    @Volatile
    private var wifiNetworks: Set<Network> = emptySet()

    private var callback: ConnectivityManager.NetworkCallback? = null

    fun init(context: Context) {
        if (connectivity != null) return
        synchronized(lock) {
            if (connectivity == null) {
                connectivity = context.applicationContext
                    .getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager
            }
        }
    }

    /** 开始监听 Wi-Fi 变化。重复调用不会重复注册。 */
    fun start(context: Context, listener: Listener) {
        init(context)
        synchronized(lock) {
            if (callback != null) return
            val cm = connectivity ?: return
            val request = NetworkRequest.Builder()
                .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
                .build()
            val cb = object : ConnectivityManager.NetworkCallback() {
                override fun onAvailable(network: Network) {
                    wifiNetworks = wifiNetworks + network
                    listener.onWifiAvailable(network)
                }

                override fun onLost(network: Network) {
                    wifiNetworks = wifiNetworks - network
                    listener.onWifiLost(network)
                }
            }
            try {
                cm.registerNetworkCallback(request, cb)
                callback = cb
                Log.i(TAG, "Wi-Fi 网络监听已启动")
            } catch (e: Exception) {
                Log.w(TAG, "Wi-Fi 网络监听注册失败", e)
            }
        }
    }

    fun stop() {
        synchronized(lock) {
            val cb = callback ?: return
            callback = null
            wifiNetworks = emptySet()
            try {
                connectivity?.unregisterNetworkCallback(cb)
            } catch (e: Exception) {
                Log.w(TAG, "Wi-Fi 网络监听注销失败：${e.message}")
            }
        }
    }

    /**
     * 当前应绑定的 Wi-Fi 网络。优先系统默认路由（若它本身是 Wi-Fi），
     * 否则从在线网络里挑一张 Wi-Fi——VPN 场景下默认路由是 VPN，
     * 底层 Wi-Fi 仍能被这里选中。
     */
    fun currentNetwork(): Network? {
        val cm = connectivity ?: return null
        return try {
            val active = cm.activeNetwork
            if (active != null && isWifi(cm, active)) {
                active
            } else {
                wifiNetworks.firstOrNull { isWifi(cm, it) }
                    ?: cm.allNetworks.firstOrNull { isWifi(cm, it) }
            }
        } catch (e: Exception) {
            Log.w(TAG, "查询 Wi-Fi 网络失败：${e.message}")
            null
        }
    }

    /**
     * 把 socket 绑定到指定网络。失败返回 false，调用方可回退默认路由。
     */
    fun bind(socket: DatagramSocket, network: Network): Boolean = try {
        network.bindSocket(socket)
        true
    } catch (e: Exception) {
        Log.w(TAG, "UDP socket 绑定 Wi-Fi 网络失败：${e.message}")
        false
    }

    /** 默认路由是否为蜂窝：无 Wi-Fi 时据此拒绝把音频流交给移动数据 */
    fun isCellularDefault(): Boolean {
        val cm = connectivity ?: return false
        return try {
            val active = cm.activeNetwork ?: return false
            cm.getNetworkCapabilities(active)
                ?.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) == true
        } catch (e: Exception) {
            Log.w(TAG, "查询默认网络失败：${e.message}")
            false
        }
    }

    private fun isWifi(cm: ConnectivityManager, network: Network): Boolean =
        try {
            cm.getNetworkCapabilities(network)
                ?.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) == true
        } catch (_: Exception) {
            false
        }
}
