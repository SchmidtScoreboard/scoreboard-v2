//! Power-on self-test (POST): deliberately reach the panel's worst-case draw
//! at the SAFEST possible moment — boot, radio still down, no flash write in
//! flight — disguised as a boot animation (board-core's gradient-to-white).
//! A USB-C port too weak for the panel browns out HERE, where the reset can't
//! corrupt anything, instead of days later mid-Wi-Fi-burst or mid-OTA-write.
//! A marker bracketing the test survives the brownout: finding it on the next
//! boot IS the diagnosis, and the board comes up in low-power mode (brightness
//! capped, warning on the panel, condition reported to the server) instead of
//! brown-out looping. The marker is cleared as soon as it's read, so any later
//! power cycle retries the test — a fixed supply restores normal operation
//! with no user action at all.

use board_core::POWER_TEST_DURATION_MS;
use embassy_time::{Duration, Instant, Timer};
use embedded_storage::{ReadStorage, Storage};
use esp_println::println;
use esp_storage::FlashStorage;

use crate::ota::SharedFlash;
use crate::SharedBoard;

/// POST-in-progress marker. Shares the 4KB nvs sector at 0xC000 with the OTA
/// boot-attempts counter (5 bytes at 0xC000 — see ota.rs ATTEMPTS_OFFSET):
/// esp-storage programs whole sectors read-modify-erase-write, so each
/// record's writes carry the other through intact. Half a sector of clearance
/// keeps the two from ever colliding; move them in lockstep or not at all.
/// A power cut mid-write erases BOTH records, and both self-heal from blank
/// (fresh strikes, a rerun of the test) — nothing durable rides on this sector.
const MARKER_OFFSET: u32 = 0xC800;
const MARKER_MAGIC: u32 = 0x504F_5354; // "POST"

/// Extra wait past the nominal test duration: the render task samples
/// board-core at ~30fps and double-buffers the panel, so the final all-white
/// frames need a few extra frames of real time to actually reach the glass —
/// the whole test is pointless if peak drive never leaves the framebuffer.
/// board-core clamps a late finish AT peak, so the margin extends the white
/// hold; it never truncates or wraps the animation.
const SETTLE_MS: u64 = 150;

/// Read AND clear the marker in one step, before ota::boot_check runs. Set
/// means the previous boot reset inside the test window: on hardware that is
/// the supply browning out under peak draw, because nothing else is running
/// there — radio down, no flash writes in flight. (A panic inside the 3s
/// window would also read as a brownout; the cost is one capped boot, and a
/// genuinely crashing image still accrues strikes on the alternating boots,
/// so the three-strike revert still converges.)
pub fn take_marker(flash: &mut FlashStorage<'static>) -> bool {
    let mut buf = [0u8; 4];
    if flash.read(MARKER_OFFSET, &mut buf).is_err() {
        return false;
    }
    if u32::from_le_bytes(buf) != MARKER_MAGIC {
        return false;
    }
    // Cleared immediately so the NEXT reset — whatever its cause — retries
    // the test: that retry is the automatic recovery path once the user swaps
    // in a real 5V 3A supply.
    clear_marker(flash);
    true
}

fn set_marker(flash: &mut FlashStorage<'static>) -> bool {
    flash.write(MARKER_OFFSET, &MARKER_MAGIC.to_le_bytes()).is_ok()
}

fn clear_marker(flash: &mut FlashStorage<'static>) {
    if flash.write(MARKER_OFFSET, &[0u8; 4]).is_err() {
        // A stuck marker means the next boot runs capped despite a healthy
        // supply — annoying, self-clearing on the boot after (take_marker
        // retries the erase), and not worth more machinery than a log line.
        println!("[post] marker clear failed — next boot may start power-limited");
    }
}

/// Run the boot POST: marker down, gradient-to-full-white for
/// POWER_TEST_DURATION_MS at peak drive, marker up. Must be called strictly
/// BEFORE any radio init, with the render task already spawned (the panel is
/// the load — an undrawn test proves nothing).
pub async fn run_test(flash: &'static SharedFlash, board: &'static SharedBoard) {
    // Marker first, and it must stick: without the tripwire a brownout would
    // reboot into the test again, forever — a board that never reaches
    // operation. Skipping the test entirely is the safer failure.
    if !set_marker(&mut *flash.lock().await) {
        println!("[post] marker write failed — skipping the power test");
        return;
    }
    {
        // Anchor the animation on a REAL clock sample: start_power_test pins
        // it to board time, and the render task may not have ticked yet — an
        // anchor of 0 would fast-forward the ramp and soften the very peak
        // the test exists to reach.
        let mut b = board.lock().await;
        b.tick(Instant::now().as_millis());
        b.start_power_test();
    }
    println!("[post] power test: ramping panel to full white over {POWER_TEST_DURATION_MS}ms");
    Timer::after(Duration::from_millis(POWER_TEST_DURATION_MS + SETTLE_MS)).await;
    board.lock().await.finish_power_test();
    // Cleared only now, with the overlay down and the panel back at user
    // brightness: the marker must cover every instant of elevated draw. (Both
    // marker writes sit outside the animation on purpose — a sector program
    // stalls all flash-resident code, render task included, and that hitch
    // lands invisibly on a static screen.)
    clear_marker(&mut *flash.lock().await);
    println!("[post] supply survived peak draw");
}

/// The marker was found: this supply already failed one test, so skip it —
/// rerunning would just brown out into a loop — and run capped instead.
pub async fn enter_low_power(board: &'static SharedBoard) {
    let mut b = board.lock().await;
    // Same real-clock anchoring as run_test: set_power_limited arms the 30s
    // warning window off board time, which may still be 0 pre-first-tick.
    b.tick(Instant::now().as_millis());
    b.set_power_limited(true);
}
