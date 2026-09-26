package com.markschmidt.scoreboard

import android.graphics.Bitmap
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.withFrameMillis
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.FilterQuality
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import kotlin.math.roundToInt

/**
 * A 64×32 panel frame, drawn the way the panel draws it: hard pixel edges
 * (nearest-neighbor — smoothing melts an LED matrix into a gradient), black
 * ground, a hairline edge so an all-dark frame still reads as a panel rather
 * than a hole.
 *
 * `selected` rings it with the accent for pickers. The ring is a RECTANGLE
 * because the panel is one; a rounded ring around a hard-edged frame reads as
 * two shapes fighting.
 *
 * A frame taller than the panel is drawn as the PANEL shows it: with a scroll
 * dwell it glides on the board's own curve (the core's `scrollWindowOffset`:
 * hold, glide down, hold, glide up, hold, one pass per dwell) with its pinned
 * title rows held still on top; without one, its top 32 rows, at rest.
 */
@Composable
fun LedFrame(
    bitmap: Bitmap?,
    modifier: Modifier = Modifier,
    selected: Boolean = false,
    accent: Color = Color(0xFF0A84FF),
    dwellMs: UInt = 0u,
    pinRows: UInt = 0u,
) {
    Box(
        modifier
            .fillMaxWidth()
            .aspectRatio(2f)
            .background(Color.Black)
            .border(if (selected) 2.dp else 1.dp, if (selected) accent else Color.White.copy(alpha = 0.10f)),
    ) {
        if (bitmap != null && bitmap.height > Frames.PANEL_HEIGHT) {
            PanelWindow(bitmap, dwellMs, pinRows)
        } else if (bitmap != null) {
            Image(
                bitmap = bitmap.asImageBitmap(),
                contentDescription = null,
                filterQuality = FilterQuality.None,
                contentScale = ContentScale.FillBounds,
                modifier = Modifier.fillMaxWidth().aspectRatio(2f),
            )
        }
    }
}

/** A tall strip's 64x32 window: rows 0 until `pinRows` from the top of the
 *  frame, the rest from `offset + pinRows` on, where the offset follows the
 *  board's curve over `dwellMs` (0 = never moves: the panel at rest). */
@Composable
private fun PanelWindow(bitmap: Bitmap, dwellMs: UInt, pinRows: UInt) {
    val image = remember(bitmap) { bitmap.asImageBitmap() }
    var elapsed by remember(bitmap) { mutableLongStateOf(0L) }
    if (dwellMs > 0u) {
        LaunchedEffect(bitmap, dwellMs) {
            val start = withFrameMillis { it }
            while (true) withFrameMillis { elapsed = it - start }
        }
    }
    val offset = uniffi.app_core.scrollWindowOffset(elapsed.toULong(), dwellMs, bitmap.height.toUInt()).toInt()
    val pin = if (dwellMs > 0u) pinRows.toInt().coerceIn(0, Frames.PANEL_HEIGHT) else 0
    Canvas(Modifier.fillMaxWidth().aspectRatio(2f)) {
        val scale = size.width / Frames.WIDTH
        fun rows(n: Int) = (n * scale).roundToInt()
        val w = size.width.roundToInt()
        if (pin > 0) {
            drawImage(
                image, srcOffset = IntOffset(0, 0), srcSize = IntSize(Frames.WIDTH, pin),
                dstOffset = IntOffset(0, 0), dstSize = IntSize(w, rows(pin)),
                filterQuality = FilterQuality.None,
            )
        }
        val body = Frames.PANEL_HEIGHT - pin
        drawImage(
            image, srcOffset = IntOffset(0, offset + pin), srcSize = IntSize(Frames.WIDTH, body),
            dstOffset = IntOffset(0, rows(pin)), dstSize = IntSize(w, rows(Frames.PANEL_HEIGHT) - rows(pin)),
            filterQuality = FilterQuality.None,
        )
    }
}
