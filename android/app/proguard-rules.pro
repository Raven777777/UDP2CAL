-keep class com.udp2cal.app.native.OpusNative { *; }
-keepclassmembers class com.udp2cal.app.native.OpusNative { *; }

# 协议层 JNI（protocol/jni.rs）：符号名由类名+方法名决定
# （Java_com_udp2cal_app_native_ProtocolNative_*），混淆/裁剪任何一个都会
# 变成运行时 UnsatisfiedLinkError: native method not found。
-keep class com.udp2cal.app.native.ProtocolNative { *; }
-keepclassmembers class com.udp2cal.app.native.ProtocolNative { *; }
-keepattributes *Annotation*
-keep class kotlin.Metadata { *; }
-dontwarn javax.annotation.**
-dontwarn kotlinx.coroutines.**
