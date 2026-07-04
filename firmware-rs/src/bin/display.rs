//! Display spike (Gate 1, PASSED) — lights the MatrixPortal S3 panel via esp-hub75.
//! Kept as a standalone binary: `cargo run --release --bin display`.

#![no_std]
#![no_main]

use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, MonoTextStyleBuilder},
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::{Alignment, Text},
};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::Pin;
use esp_hal::main;
use esp_hal::time::Rate;
use esp_hub75::{
    framebuffer::{bitplane::plain::DmaFrameBuffer, compute_rows},
    Color, Hub75, Hub75Pins16,
};

esp_bootloader_esp_idf::esp_app_desc!();

const ROWS: usize = 32;
const COLS: usize = 64;
const NROWS: usize = compute_rows(ROWS);
const PLANES: usize = 7;
type FBType = DmaFrameBuffer<NROWS, COLS, PLANES>;

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    let tx_descriptors = esp_hub75::hub75_dma_descriptors!(FBType);

    let pins = Hub75Pins16 {
        red1: peripherals.GPIO42.degrade(),
        grn1: peripherals.GPIO41.degrade(),
        blu1: peripherals.GPIO40.degrade(),
        red2: peripherals.GPIO38.degrade(),
        grn2: peripherals.GPIO39.degrade(),
        blu2: peripherals.GPIO37.degrade(),
        addr0: peripherals.GPIO45.degrade(),
        addr1: peripherals.GPIO36.degrade(),
        addr2: peripherals.GPIO48.degrade(),
        addr3: peripherals.GPIO35.degrade(),
        addr4: peripherals.GPIO21.degrade(),
        blank: peripherals.GPIO14.degrade(),
        clock: peripherals.GPIO2.degrade(),
        latch: peripherals.GPIO47.degrade(),
    };

    let mut hub75 = Hub75::new_async(
        peripherals.LCD_CAM,
        pins,
        peripherals.DMA_CH0,
        tx_descriptors,
        Rate::from_mhz(20),
    )
    .expect("failed to create Hub75!");

    let mut fb = FBType::new();
    let mut bar = |x: i32, c: Color| {
        Rectangle::new(Point::new(x, 0), Size::new(16, 20))
            .into_styled(PrimitiveStyle::with_fill(c))
            .draw(&mut fb)
            .ok();
    };
    bar(0, Color::RED);
    bar(16, Color::GREEN);
    bar(32, Color::BLUE);
    bar(48, Color::WHITE);

    let style = MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(Color::YELLOW)
        .build();
    Text::with_alignment("RUST OK", Point::new(32, 28), style, Alignment::Center)
        .draw(&mut fb)
        .ok();

    loop {
        let xfer = hub75.render(&fb).map_err(|(e, _)| e).expect("render");
        let (result, new) = xfer.wait();
        hub75 = new;
        result.expect("transfer");
    }
}
