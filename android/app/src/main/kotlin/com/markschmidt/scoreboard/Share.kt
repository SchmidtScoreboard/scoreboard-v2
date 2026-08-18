package com.markschmidt.scoreboard

import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import androidx.core.content.FileProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.File

/**
 * Sharing a frame: each panel pixel drawn as a lit square with a dark gutter
 * on a black bezel — the picture of an LED matrix, not a blown-up bitmap. A
 * nearest-neighbour upscale reads as low-resolution clip art; the gutter is
 * what makes it read as the physical board.
 *
 * Geometry matches the iOS export exactly (12px pitch, 10px dot, one-LED
 * bezel) so a frame shared from either phone looks like the same object.
 */
object Share {
    private const val PITCH = 12
    private const val DOT = 10
    private const val BEZEL = 12

    fun ledImage(rgba: ByteArray): Bitmap? {
        val src = Frames.toBitmap(rgba) ?: return null
        return ledImageFrom(src)
    }

    fun ledImageFrom(src: Bitmap): Bitmap {
        val out = Bitmap.createBitmap(
            src.width * PITCH + 2 * BEZEL,
            src.height * PITCH + 2 * BEZEL,
            Bitmap.Config.ARGB_8888,
        )
        val canvas = Canvas(out)
        canvas.drawColor(Color.BLACK)
        val paint = Paint()
        val inset = (PITCH - DOT) / 2f
        for (y in 0 until src.height) {
            for (x in 0 until src.width) {
                val c = src.getPixel(x, y)
                // An unlit LED is part of the bezel — drawing black on black
                // is a no-op, so skip it.
                if (c and 0x00FFFFFF == 0) continue
                paint.color = c
                val left = BEZEL + x * PITCH + inset
                val top = BEZEL + y * PITCH + inset
                canvas.drawRect(left, top, left + DOT, top + DOT, paint)
            }
        }
        return out
    }

    /** Hand the frame to the system share sheet. */
    suspend fun share(context: Context, rgba: ByteArray, title: String) {
        val bmp = ledImage(rgba) ?: return
        shareLed(context, bmp, title)
    }

    /** Same, from an already-decoded 64×32 frame (the HTTP render path). */
    suspend fun shareBitmap(context: Context, frame: Bitmap, title: String) =
        shareLed(context, ledImageFrom(frame), title)

    private suspend fun shareLed(context: Context, bmp: Bitmap, title: String) {
        val uri = withContext(Dispatchers.IO) {
            val dir = File(context.cacheDir, "shared").apply { mkdirs() }
            val file = File(dir, "scoreboard.png")
            file.outputStream().use { bmp.compress(Bitmap.CompressFormat.PNG, 100, it) }
            FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", file)
        }
        val intent = Intent(Intent.ACTION_SEND).apply {
            type = "image/png"
            putExtra(Intent.EXTRA_STREAM, uri)
            putExtra(Intent.EXTRA_TITLE, title)
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
        context.startActivity(Intent.createChooser(intent, title))
    }
}
