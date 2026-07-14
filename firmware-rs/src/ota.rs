//! Over-the-air firmware updates.
//!
//! Flow: the server pushes `ServerMsg::UpdateFirmware { version, size, sha256,
//! sig, path }`; the device task drops its websocket and calls
//! [`download_and_install`], which streams `path` (the offered release
//! channel's image, e.g. "/firmware/stable.bin") from the SAME
//! server into the inactive OTA slot, verifying byte count, sha256, and an
//! Ed25519 signature (against the baked-in [`FW_PUBKEY_HEX`]) as it goes. Only
//! after ALL THREE checks pass is otadata flipped to the new slot. Any failure
//! leaves otadata untouched — the running image keeps running and the whole
//! attempt is a non-event.
//!
//! Self-confirm + revert, bootloader-INDEPENDENT: the stock espflash
//! bootloader is NOT assumed to implement esp-idf's auto-rollback, so the
//! firmware polices itself. A freshly installed slot is marked
//! [`OtaImageState::New`]; on the next boot [`boot_check`] sees
//! New/PendingVerify (PendingVerify covers a rollback-capable bootloader that
//! did the New->PendingVerify flip) and reports "unconfirmed", and main spawns
//! [`revert_watchdog`]. The image confirms itself by completing one websocket
//! auth — the entire job of this board is talking to its server, so "reached
//! the server" is the strongest health signal available —
//! [`confirm_running_image`] then writes `Valid`. If five minutes pass without
//! that, the watchdog flips otadata back to the previous slot (still intact:
//! installs only ever write the INACTIVE slot) and reboots.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use ed25519_dalek::{Signature, VerifyingKey};
use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;
use embedded_storage::{ReadStorage, Storage};
use esp_bootloader_esp_idf::ota::OtaImageState;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::{
    read_partition_table, AppPartitionSubType, PartitionType, PARTITION_TABLE_MAX_LEN,
};
use esp_println::println;
use esp_storage::FlashStorage;
use sha2::{Digest, Sha256};

use crate::fw_pubkey::FW_PUBKEY_HEX;
use crate::SharedBoard;

/// The flash peripheral, shared between the brightness/reset task and the OTA
/// updater. Every user locks per SHORT operation (one sector write, one
/// otadata read/write) so hold-to-reset stays responsive through a download —
/// the longest anything holds the lock is a single 4KB sector program.
pub type SharedFlash = Mutex<CriticalSectionRawMutex, FlashStorage<'static>>;

/// One update offer, as received in `ServerMsg::UpdateFirmware`.
pub struct UpdateJob {
    pub version: String,
    pub size: u32,
    pub sha256: String,
    pub sig: String,
    /// Claimed release channel — bound into the signature, so it is authentic
    /// once verification passes. NOTE the residual gap (accepted for now): the
    /// device does not persist its own enrollment, so an on-path attacker on
    /// the plaintext ws can offer a stable board an AUTHENTIC dev image. Every
    /// installable image is still one we signed, strictly newer, and covered
    /// by the revert machinery.
    pub channel: String,
    /// Server-relative path of the image to GET (e.g. "/firmware/beta.bin") —
    /// the server names the release channel the offered dist lives in.
    /// Validated by [`valid_path`] before any I/O.
    pub path: String,
}

/// A plausible request path: absolute, bounded, printable ASCII with no
/// whitespace/control bytes — anything else would splice garbage into the
/// HTTP request line, so it's rejected before the socket ever opens.
fn valid_path(p: &str) -> bool {
    p.starts_with('/') && p.len() <= 128 && p.bytes().all(|b| b.is_ascii_graphic())
}

/// Set once this boot's image has proven itself (first successful ws auth).
/// The revert watchdog checks it at the five-minute mark.
static FW_CONFIRMED: AtomicBool = AtomicBool::new(false);

/// Flash sector size (the OTA slots start 64KB-aligned).
const CHUNK: usize = 4096;
/// Writes are batched into bursts of this many bytes. Every flash program
/// disables the SPI cache, stalling all code that executes from flash —
/// including the render task that re-arms the panel's DMA — so the panel goes
/// DARK for the duration of each write. Writing per 4KB sector as bytes
/// arrived strobed the panel at ~16Hz for the whole download (measured off a
/// video). One 32KB burst darkens the panel once (~200ms), and the lit window
/// after it makes the whole update read as a slow deliberate pulse instead.
const BURST: usize = CHUNK * 8;
/// Panel-visible pause after each burst: long enough to read as "on, working",
/// showing the freshly advanced progress bar.
const LIT_WINDOW_MS: u64 = 600;

/// Boot-attempt counter for unconfirmed images: its own nvs sector (after
/// creds 0x9000 / brightness 0xA000 / identity 0xB000). Incremented by
/// `boot_check` on every unconfirmed boot, cleared on confirm — combined with
/// the panic handler REBOOTING instead of halting, a signed image that panics
/// during early init boot-loops into this counter and reverts after three
/// strikes. This is the case the 5-minute watchdog can't reach: a halted CPU
/// runs no tasks.
const ATTEMPTS_OFFSET: u32 = 0xC000;
const ATTEMPTS_MAGIC: u32 = 0x4F54_4131; // "OTA1"
const MAX_BOOT_ATTEMPTS: u8 = 3;

fn read_attempts(flash: &mut FlashStorage<'static>) -> u8 {
    let mut buf = [0u8; 5];
    if flash.read(ATTEMPTS_OFFSET, &mut buf).is_err() {
        return 0;
    }
    if u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) != ATTEMPTS_MAGIC {
        return 0;
    }
    buf[4]
}

fn write_attempts(flash: &mut FlashStorage<'static>, n: u8) {
    let mut buf = [0u8; 5];
    buf[..4].copy_from_slice(&ATTEMPTS_MAGIC.to_le_bytes());
    buf[4] = n;
    let _ = flash.write(ATTEMPTS_OFFSET, &buf);
}

/// Flip otadata to the other slot, condemn this one, and reboot. The shared
/// revert primitive for the watchdog, the three-strike counter, and an
/// interrupted revert found as `Invalid` at boot.
fn revert_now(flash: &mut FlashStorage<'static>) -> ! {
    let mut buf = table_buf();
    if let Ok(mut up) = OtaUpdater::new(flash, as_table_arr(&mut buf)) {
        let _ = up.set_current_ota_state(OtaImageState::Invalid);
        let _ = up.activate_next_partition();
        let _ = up.set_current_ota_state(OtaImageState::Valid);
    }
    write_attempts(flash, 0);
    esp_hal::system::software_reset()
}

/// Scratch for the crate's partition-table reads (3KB — heap, not task stack).
fn table_buf() -> Vec<u8> {
    alloc::vec![0u8; PARTITION_TABLE_MAX_LEN]
}

fn as_table_arr(buf: &mut [u8]) -> &mut [u8; PARTITION_TABLE_MAX_LEN] {
    // Infallible: `table_buf` allocates exactly PARTITION_TABLE_MAX_LEN.
    buf.try_into().unwrap()
}

/// Boot-time otadata reconciliation. Returns true when the RUNNING image is a
/// fresh, unconfirmed OTA install (state New/PendingVerify) — the caller then
/// spawns [`revert_watchdog`] and confirms on the first ws auth.
///
/// Also repairs otadata so updates stay safe:
/// - otadata blank (first wired flash — scripts/flash.sh erases it): the
///   bootloader fell back to booting ota_0 while otadata says "Factory". The
///   crate's next-slot math misbehaves from Factory (and could point AT the
///   running slot), so adopt the booted slot as current+Valid; "next" is then
///   always the other, inactive slot.
/// - otadata selects a slot we are NOT running from: the bootloader must have
///   refused the selected slot and fallen back. Re-point otadata at the slot
///   that actually works (us) so the next reboot doesn't retry the bad one.
///
/// Any read failure returns false — no OTA partitions just means the feature
/// is off, never a scary state.
pub fn boot_check(flash: &mut FlashStorage<'static>) -> bool {
    let mut buf = table_buf();
    // Which slot are we physically executing from? (MMU-derived; None if the
    // crate can't tell — then we skip the fallback repair and trust otadata.)
    let booted = {
        match read_partition_table(flash, &mut buf[..]) {
            Ok(pt) => match pt.booted_partition() {
                Ok(Some(entry)) => match entry.partition_type() {
                    PartitionType::App(slot) => Some(slot),
                    _ => None,
                },
                _ => None,
            },
            Err(_) => return false,
        }
    };
    let Ok(mut up) = OtaUpdater::new(flash, as_table_arr(&mut buf)) else {
        return false;
    };
    let Ok(selected) = up.selected_partition() else {
        return false;
    };
    if selected == AppPartitionSubType::Factory {
        // Blank otadata: adopt the running slot (see doc comment).
        let slot = booted.unwrap_or(AppPartitionSubType::Ota0);
        if let Ok(mut ota) = up.ota_data() {
            let _ = ota.set_current_app_partition(slot);
            let _ = ota.set_current_ota_state(OtaImageState::Valid);
            println!("[ota] adopted running slot {slot:?} into blank otadata");
        }
        return false;
    }
    if let Some(b) = booted {
        if b != selected {
            // Bootloader fallback: the selected slot didn't boot; we did.
            if let Ok(mut ota) = up.ota_data() {
                let _ = ota.set_current_app_partition(b);
                let _ = ota.set_current_ota_state(OtaImageState::Valid);
                println!("[ota] otadata pointed at {selected:?} but {b:?} booted — repaired");
            }
            return false;
        }
    }
    match up.current_ota_state() {
        Ok(OtaImageState::New) | Ok(OtaImageState::PendingVerify) => {
            // Three strikes: a panic-looping image never keeps the watchdog
            // alive long enough to revert, but (with the panic handler
            // REBOOTING rather than halting) it re-enters this early check on
            // every crash. Count the attempts; on the third, give up on the
            // new image and boot the previous one.
            let attempts = read_attempts(flash).saturating_add(1);
            if attempts >= MAX_BOOT_ATTEMPTS {
                println!("[ota] unconfirmed image failed {attempts} boots — reverting");
                revert_now(flash);
            }
            write_attempts(flash, attempts);
            println!("[ota] running an unconfirmed OTA image (boot attempt {attempts})");
            true
        }
        // A power loss between the watchdog's Invalid mark and its otadata
        // flip strands the condemned slot as the boot target — finish the
        // interrupted revert instead of running a known-bad image.
        Ok(OtaImageState::Invalid) => {
            println!("[ota] booted a slot condemned by an interrupted revert — completing it");
            revert_now(flash);
        }
        // Undefined/Valid/other (or unreadable): treat as confirmed. Undefined
        // can also appear if power died between commit()'s two otadata writes;
        // reverting to a possibly-stale other slot on that guess would be
        // worse than trusting the image the bootloader just ran.
        _ => {
            if read_attempts(flash) != 0 {
                write_attempts(flash, 0); // stale strikes from a confirmed past
            }
            false
        }
    }
}

/// Mark the running image Valid — called after the first successful ws auth of
/// an unconfirmed boot. Idempotent per boot; also stands the watchdog down.
pub async fn confirm_running_image(flash: &'static SharedFlash) {
    if FW_CONFIRMED.swap(true, Ordering::AcqRel) {
        return;
    }
    let mut f = flash.lock().await;
    let mut buf = table_buf();
    match OtaUpdater::new(&mut *f, as_table_arr(&mut buf)) {
        Ok(mut up) => match up.set_current_ota_state(OtaImageState::Valid) {
            Ok(()) => println!("[ota] image confirmed (auth reached the server)"),
            Err(_) => println!("[ota] failed to persist Valid state"),
        },
        Err(_) => println!("[ota] no OTA partitions; skipping confirm"),
    }
    write_attempts(&mut f, 0);
}

/// Revert watchdog for an unconfirmed OTA boot. The new image gets five
/// minutes to complete one websocket auth (Wi-Fi join + DHCP + DNS + connect —
/// normally seconds; five minutes forgives a slow router reboot). If it
/// doesn't, we assume the update broke connectivity — the one thing this board
/// exists for — flip otadata back to the previous slot, and reboot into it.
/// The previous image is guaranteed intact: installs only write the INACTIVE
/// slot, and this task exists precisely because the bootloader can't be
/// trusted to do the rollback itself.
#[embassy_executor::task]
pub async fn revert_watchdog(flash: &'static SharedFlash) {
    Timer::after(Duration::from_secs(5 * 60)).await;
    if FW_CONFIRMED.load(Ordering::Acquire) {
        return;
    }
    println!("[ota] no server auth within 5min of an unconfirmed boot — reverting");
    {
        let mut f = flash.lock().await;
        let mut buf = table_buf();
        if let Ok(mut up) = OtaUpdater::new(&mut *f, as_table_arr(&mut buf)) {
            // Mark ourselves Invalid FIRST (a rollback-capable bootloader will
            // also refuse this slot), then flip: with two slots, "next" IS the
            // previous image. Re-affirm it Valid so nothing re-reverts.
            let _ = up.set_current_ota_state(OtaImageState::Invalid);
            let _ = up.activate_next_partition();
            let _ = up.set_current_ota_state(OtaImageState::Valid);
        }
    }
    esp_hal::system::software_reset();
}

struct Target {
    /// Absolute flash offset of the inactive slot.
    offset: u32,
    slot: AppPartitionSubType,
}

/// Resolve the slot the download writes into: the one `activate_next_partition`
/// would pick, sanity-checked to never be the slot we're executing from
/// (erasing the running image's sectors is a brick until reflash).
async fn resolve_target(flash: &SharedFlash, size: u32) -> Result<Target, &'static str> {
    let mut f = flash.lock().await;
    let mut buf = table_buf();
    let slot = {
        let mut up = OtaUpdater::new(&mut *f, as_table_arr(&mut buf))
            .map_err(|_| "no OTA partition table")?;
        if up.selected_partition().map_err(|_| "otadata unreadable")?
            == AppPartitionSubType::Factory
        {
            // boot_check adopts the running slot at every boot, so this should
            // be unreachable — but the crate's next-slot arithmetic underflows
            // from Factory, so refuse loudly rather than compute garbage.
            return Err("otadata uninitialized");
        }
        let (_region, slot) = up.next_partition().map_err(|_| "no inactive slot")?;
        slot
    };
    // Second pass over the table for the slot's absolute offset/capacity (the
    // updater keeps its table private) and the running-slot paranoia check.
    let pt = read_partition_table(&mut *f, &mut buf[..])
        .map_err(|_| "partition table unreadable")?;
    let entry = pt
        .find_partition(PartitionType::App(slot))
        .ok()
        .flatten()
        .ok_or("target slot missing from the table")?;
    if size == 0 || size > entry.len() {
        return Err("image size does not fit the OTA slot");
    }
    if let Ok(Some(booted)) = pt.booted_partition() {
        if booted.partition_type() == PartitionType::App(slot) {
            return Err("target slot is the running image");
        }
    }
    Ok(Target { offset: entry.offset(), slot })
}

/// Flip otadata to the freshly written slot and mark it New. Only called after
/// every verification passed; re-checks that "next" is still the slot we wrote.
async fn commit(flash: &SharedFlash, expected: AppPartitionSubType) -> Result<(), &'static str> {
    let mut f = flash.lock().await;
    let mut buf = table_buf();
    let mut up =
        OtaUpdater::new(&mut *f, as_table_arr(&mut buf)).map_err(|_| "no OTA partition table")?;
    let (_region, slot) = up.next_partition().map_err(|_| "no inactive slot")?;
    if slot != expected {
        return Err("active slot changed during the download");
    }
    up.activate_next_partition().map_err(|_| "otadata activate failed")?;
    // New = "unconfirmed": the next boot arms the revert watchdog. The two
    // otadata writes aren't atomic; power loss between them leaves Undefined,
    // which boot_check deliberately reads as confirmed (see there).
    up.set_current_ota_state(OtaImageState::New)
        .map_err(|_| "otadata state write failed")?;
    Ok(())
}

/// Burst-buffered writer into the inactive slot. Accumulates BURST bytes in
/// RAM, programs them in one flash lock (~200ms — hold-to-reset only touches
/// flash AFTER its 10s gesture, so this never blocks the escape hatch), then
/// rests for LIT_WINDOW_MS with the panel visible. See BURST for why: each
/// program stalls the panel dark, and per-sector writes strobed it.
struct SlotWriter<'a> {
    flash: &'a SharedFlash,
    base: u32,
    chunk: Vec<u8>,
    buffered: usize,
    written: usize,
}

impl SlotWriter<'_> {
    async fn feed(&mut self, mut data: &[u8]) -> Result<(), &'static str> {
        while !data.is_empty() {
            let take = (BURST - self.buffered).min(data.len());
            self.chunk[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered == BURST {
                self.flush().await?;
                // The lit window: the panel shows the just-advanced bar while
                // the socket backpressures. Skipped by the final short flush.
                Timer::after(Duration::from_millis(LIT_WINDOW_MS)).await;
            }
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), &'static str> {
        if self.buffered > 0 {
            self.flash
                .lock()
                .await
                .write(self.base + self.written as u32, &self.chunk[..self.buffered])
                .map_err(|_| "flash write failed")?;
            self.written += self.buffered;
            self.buffered = 0;
        }
        Ok(())
    }
}

/// Download the offer's `path` from the server we were just connected to,
/// verify it (length, sha256, Ed25519 against the baked-in pubkey), write it
/// into the inactive OTA slot, and flip otadata. `Ok(())` means "reboot me";
/// any `Err` leaves otadata untouched and the caller resumes normal operation
/// — a failed update must be a non-event.
///
/// Runs in the device task with the websocket already dropped: it borrows the
/// same rx/tx socket buffers (like `fetch_timezone`), and progress renders
/// from the DEVICE's own byte counter via `set_updating` — nothing animates
/// over the network.
#[allow(clippy::too_many_arguments)]
pub async fn download_and_install(
    stack: Stack<'static>,
    rx: &mut [u8],
    tx: &mut [u8],
    ip: core::net::Ipv4Addr,
    port: u16,
    host: &str,
    job: &UpdateJob,
    flash: &'static SharedFlash,
    board: &'static SharedBoard,
) -> Result<(), &'static str> {
    // Downgrade/replay guard: the plain-HTTP channel means anyone on-path can
    // replay an OLD signed release as an "update" — refuse anything that isn't
    // strictly newer than what we're running.
    if !scoreboard_protocol::version_newer(&job.version, env!("CARGO_PKG_VERSION")) {
        return Err("offered version is not newer than the running one");
    }
    // The download path is server-supplied: sanity-check it before it goes
    // anywhere near the request line.
    if !valid_path(&job.path) {
        return Err("bad download path in the offer");
    }
    board.lock().await.set_updating(Some(0));

    // Decode key + signature before any I/O — reject garbage offers instantly.
    let vk = decode_pubkey().ok_or("bad baked-in pubkey")?;
    let sig = decode_sig(&job.sig).ok_or("bad signature encoding")?;
    let mut verifier = vk.verify_stream(&sig).map_err(|_| "bad signature")?;
    // The signature binds version and size alongside the bytes (same prefix
    // sign-firmware feeds the signer) — a signed IMAGE with a doctored offer
    // must not verify.
    if !matches!(job.channel.as_str(), "stable" | "beta" | "dev") {
        return Err("offer names an unknown channel");
    }
    let prefix =
        alloc::format!("scoreboard-fw:{}:{}:{}:", job.version, job.size, job.channel);
    verifier.update(prefix.as_bytes());
    let size = job.size as usize;

    let target = resolve_target(flash, job.size).await?;
    println!(
        "[ota] downloading v{} from {} ({size} bytes) into {:?} @ {:#x}",
        job.version, job.path, target.slot, target.offset
    );

    let mut sock = TcpSocket::new(stack, rx, tx);
    sock.set_timeout(Some(Duration::from_secs(30)));
    sock.connect((ip, port)).await.map_err(|_| "tcp connect failed")?;
    let mut req = Vec::with_capacity(192);
    req.extend_from_slice(b"GET ");
    req.extend_from_slice(job.path.as_bytes());
    req.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    req.extend_from_slice(host.as_bytes());
    req.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
    sock.write_all(&req).await.map_err(|_| "http send failed")?;

    // Headers: tiny and bounded; require a 200 before touching flash.
    let mut net = alloc::vec![0u8; 1024];
    let mut hdr: Vec<u8> = Vec::with_capacity(512);
    let body_start = loop {
        let n = sock.read(&mut net).await.map_err(|_| "read failed in headers")?;
        if n == 0 {
            return Err("connection closed in headers");
        }
        hdr.extend_from_slice(&net[..n]);
        if let Some(p) = find(&hdr, b"\r\n\r\n") {
            break p + 4;
        }
        if hdr.len() > 4096 {
            return Err("oversized response headers");
        }
    };
    let status_end = hdr.iter().position(|&b| b == b'\r').unwrap_or(hdr.len());
    if find(&hdr[..status_end], b" 200").is_none() {
        return Err("server did not return 200");
    }

    let mut out = SlotWriter {
        flash,
        base: target.offset,
        chunk: alloc::vec![0u8; BURST],
        buffered: 0,
        written: 0,
    };
    let mut hasher = Sha256::new();
    let mut last_pct = 0u8;

    // Body bytes that rode in with the headers, then the socket until exactly
    // `size` bytes. More than `size` is as fatal as fewer: the image on the
    // wire must be byte-identical to the one that was signed.
    let first = &hdr[body_start..];
    if first.len() > size {
        return Err("image larger than advertised");
    }
    hasher.update(first);
    verifier.update(first);
    out.feed(first).await?;
    let mut received = first.len();

    while received < size {
        let n = sock.read(&mut net).await.map_err(|_| "read failed mid-download")?;
        if n == 0 {
            return Err("connection closed mid-download");
        }
        if received + n > size {
            return Err("image larger than advertised");
        }
        let data = &net[..n];
        hasher.update(data);
        verifier.update(data);
        out.feed(data).await?;
        received += n;

        // Progress off our own byte counter, redrawn by the render task.
        let pct = (received as u64 * 100 / size as u64) as u8;
        if pct != last_pct {
            board.lock().await.set_updating(Some(pct));
            last_pct = pct;
        }
    }
    out.flush().await?;

    // Length matched (loop invariant). Now the digest and the signature.
    let digest_hex = hex::encode(hasher.finalize());
    if !digest_hex.eq_ignore_ascii_case(&job.sha256) {
        return Err("sha256 mismatch");
    }
    verifier
        .finalize_and_verify()
        .map_err(|_| "Ed25519 signature rejected")?;

    // All three checks passed — only now does otadata move.
    commit(flash, target.slot).await
}

fn decode_pubkey() -> Option<VerifyingKey> {
    let bytes = hex::decode(FW_PUBKEY_HEX).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&arr).ok()
}

fn decode_sig(sig_hex: &str) -> Option<Signature> {
    let bytes = hex::decode(sig_hex).ok()?;
    let arr: [u8; 64] = bytes.try_into().ok()?;
    Some(Signature::from_bytes(&arr))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
