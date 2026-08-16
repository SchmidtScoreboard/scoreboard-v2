package com.markschmidt.scoreboard

import android.graphics.Bitmap

/**
 * A server-pushed frame: RGBA8 bytes, 64 wide, height DERIVED from the byte
 * count — frames are variable height (a deep leaderboard renders up to 128
 * rows for the physical board to scroll). Hardcoding 64x32 and rejecting the
 * rest silently dropped tall frames on iOS once; don't inherit that bug.
 * Anything taller than the panel is cropped to the top 32 rows, the same
 * window the iOS cards show.
 */
object Frames {
    const val WIDTH = 64
    const val PANEL_HEIGHT = 32

    fun toBitmap(rgba: ByteArray): Bitmap? {
        val rowBytes = WIDTH * 4
        if (rgba.isEmpty() || rgba.size % rowBytes != 0) return null
        val height = minOf(rgba.size / rowBytes, PANEL_HEIGHT)
        val pixels = IntArray(WIDTH * height)
        for (i in 0 until WIDTH * height) {
            val r = rgba[i * 4].toInt() and 0xFF
            val g = rgba[i * 4 + 1].toInt() and 0xFF
            val b = rgba[i * 4 + 2].toInt() and 0xFF
            pixels[i] = (0xFF shl 24) or (r shl 16) or (g shl 8) or b
        }
        return Bitmap.createBitmap(pixels, WIDTH, height, Bitmap.Config.ARGB_8888)
    }
}
