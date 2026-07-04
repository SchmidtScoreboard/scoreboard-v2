//! Minimal Adafruit seesaw rotary-encoder driver (encoder position + push switch)
//! over esp-hal blocking I2C. Just enough for brightness + the board-core gestures.
//! Seesaw needs a short delay between the register write and the read.

use embassy_time::{Duration, Timer};
use esp_hal::i2c::master::I2c;
use esp_hal::Blocking;

const ADDR: u8 = 0x36;
const ENCODER_BASE: u8 = 0x11;
const ENCODER_POSITION: u8 = 0x30;
const GPIO_BASE: u8 = 0x01;
const GPIO_DIRCLR_BULK: u8 = 0x03;
const GPIO_BULK: u8 = 0x04;
const GPIO_BULK_SET: u8 = 0x05;
const GPIO_PULLENSET: u8 = 0x0B;
const SWITCH: u32 = 24; // the rotary-encoder breakout's push button

pub struct Encoder {
    i2c: I2c<'static, Blocking>,
}

impl Encoder {
    pub fn new(i2c: I2c<'static, Blocking>) -> Self {
        Self { i2c }
    }

    fn cmd(&mut self, base: u8, func: u8, data: &[u8]) -> bool {
        let mut buf = [0u8; 8];
        buf[0] = base;
        buf[1] = func;
        buf[2..2 + data.len()].copy_from_slice(data);
        self.i2c.write(ADDR, &buf[..2 + data.len()]).is_ok()
    }

    async fn read(&mut self, base: u8, func: u8, out: &mut [u8]) -> bool {
        if self.i2c.write(ADDR, &[base, func]).is_err() {
            return false;
        }
        Timer::after(Duration::from_micros(800)).await;
        self.i2c.read(ADDR, out).is_ok()
    }

    /// Configure the push switch as input + pullup. Returns false if the seesaw
    /// doesn't ack (no encoder attached) — caller can keep running without it.
    pub fn init(&mut self) -> bool {
        let mask = (1u32 << SWITCH).to_be_bytes();
        self.cmd(GPIO_BASE, GPIO_DIRCLR_BULK, &mask)
            && self.cmd(GPIO_BASE, GPIO_PULLENSET, &mask)
            && self.cmd(GPIO_BASE, GPIO_BULK_SET, &mask)
    }

    pub async fn position(&mut self) -> Option<i32> {
        let mut b = [0u8; 4];
        if self.read(ENCODER_BASE, ENCODER_POSITION, &mut b).await {
            Some(i32::from_be_bytes(b))
        } else {
            None
        }
    }

    /// True while the knob is pressed (the switch is active-low).
    pub async fn pressed(&mut self) -> bool {
        let mut b = [0u8; 4];
        self.read(GPIO_BASE, GPIO_BULK, &mut b).await && (u32::from_be_bytes(b) & (1 << SWITCH)) == 0
    }
}
