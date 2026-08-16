package com.markschmidt.scoreboard

import android.graphics.Bitmap
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.FilterQuality
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.unit.dp

/**
 * A 64×32 panel frame, drawn the way the panel draws it: hard pixel edges
 * (nearest-neighbor — smoothing melts an LED matrix into a gradient), black
 * ground, a hairline edge so an all-dark frame still reads as a panel rather
 * than a hole.
 *
 * `selected` rings it with the accent for pickers. The ring is a RECTANGLE
 * because the panel is one; a rounded ring around a hard-edged frame reads as
 * two shapes fighting.
 */
@Composable
fun LedFrame(
    bitmap: Bitmap?,
    modifier: Modifier = Modifier,
    selected: Boolean = false,
    accent: Color = Color(0xFF0A84FF),
) {
    Box(
        modifier
            .fillMaxWidth()
            .aspectRatio(2f)
            .background(Color.Black)
            .border(if (selected) 2.dp else 1.dp, if (selected) accent else Color.White.copy(alpha = 0.10f)),
    ) {
        if (bitmap != null) {
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
