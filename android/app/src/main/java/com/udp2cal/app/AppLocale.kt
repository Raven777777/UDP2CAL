package com.udp2cal.app

import android.content.Context
import android.content.res.Configuration
import android.os.LocaleList
import android.view.ContextThemeWrapper
import androidx.annotation.StringRes

/**
 * 应用内语言覆盖。
 *
 * 系统语言时直接使用原始 Context；用户选择语言后用 createConfigurationContext
 * 覆盖 locale。Activity 需要保留主题，所以额外套一层 ContextThemeWrapper。
 */
object AppLocale {
    const val SYSTEM = ""

    fun available(context: Context): List<Pair<String, String>> =
        context.resources.getStringArray(R.array.language_tags).map { tag ->
            tag to localized(context, tag).getString(R.string.language_name)
        }

    fun wrapActivity(base: Context): Context {
        val tag = Prefs.savedLanguage(base)
        if (tag.isEmpty()) return base
        val wrapper = ContextThemeWrapper(base, R.style.Theme_UDP2CAL)
        wrapper.applyOverrideConfiguration(config(base, tag))
        return wrapper
    }

    fun s(context: Context, @StringRes id: Int, vararg args: Any): String {
        val resources = localized(context, Prefs.savedLanguage(context)).resources
        return if (args.isEmpty()) resources.getString(id) else resources.getString(id, *args)
    }

    private fun contextFor(base: Context, tag: String): Context =
        if (tag.isEmpty()) base else base.createConfigurationContext(config(base, tag))

    private fun localized(base: Context, tag: String): Context = contextFor(base, tag)

    private fun config(base: Context, tag: String): Configuration =
        Configuration(base.resources.configuration).apply {
            // 持久化值是外部输入：旧版/手工写入的非法 tag 不能导致启动崩溃，
            // 解析失败时保持系统语言。
            runCatching { setLocales(LocaleList.forLanguageTags(tag)) }
        }
}
