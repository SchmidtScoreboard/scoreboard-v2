package com.markschmidt.scoreboard

import android.app.Application

/** Owns the one Session for the process — the core client lives as long as
 *  the app does, reconnecting on its own; activities just observe. */
class ScoreboardApplication : Application() {
    val session: Session by lazy { Session(this) }
}
