package com.markschmidt.scoreboard

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.ConcurrentHashMap

/**
 * Server-rendered preview frames (`/sample`, `/offmode`, `/guide`): the same
 * pictures the iOS settings screens show, because a picture of what the panel
 * will do beats a paragraph describing it.
 *
 * Cached in memory by URL. The server marks these day-cacheable and their
 * bytes are deterministic renders, so a repeat visit costs nothing; a failed
 * load stays null and the caller draws its own placeholder rather than an
 * error — the same "degrade quietly" rule iOS uses, since these are decoration
 * around a control that still works.
 */
object Previews {
    private val cache = ConcurrentHashMap<String, Bitmap>()

    suspend fun load(url: String): Bitmap? {
        cache[url]?.let { return it }
        return withContext(Dispatchers.IO) {
            runCatching {
                val c = (URL(url).openConnection() as HttpURLConnection).apply {
                    connectTimeout = 10_000
                    readTimeout = 10_000
                }
                val code = c.responseCode
                if (code != 200) {
                    // A silent null here is indistinguishable from "no preview
                    // declared", which is how an empty tile went unexplained.
                    android.util.Log.w("Previews", "HTTP $code for $url")
                    c.disconnect()
                    return@runCatching null
                }
                c.inputStream.use { BitmapFactory.decodeStream(it) }
            }.onFailure { android.util.Log.w("Previews", "failed $url", it) }
                .getOrNull()?.also { cache[url] = it }
        }
    }
}

/** A preview frame that loads itself, keyed on the URL. */
@Composable
fun rememberPreview(url: String?): Bitmap? {
    var bmp by remember(url) { mutableStateOf<Bitmap?>(null) }
    LaunchedEffect(url) { bmp = url?.let { Previews.load(it) } }
    return bmp
}
