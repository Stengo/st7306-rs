#![no_std]
#![allow(clippy::result_unit_err)]
// TODO: Make the config nicer, instead of ST7306::new with tons of arguments
#![allow(clippy::too_many_arguments)]

//! This crate provides a ST7306 driver to connect to TFT displays.

pub mod instruction;

use crate::instruction::Instruction;

use embedded_hal::blocking::delay::DelayMs;
use embedded_hal::blocking::spi;
use embedded_hal::digital::v2::OutputPin;

#[derive(Debug, PartialEq, Eq)]
pub enum PowerMode {
    /// High Power Mode
    Hpm,
    /// Low Power Mode
    Lpm,
}

const COL_MAX: u16 = 59;
const ROW_MAX: u16 = 200;

const PX_PER_COL: u16 = 12;
const PX_PER_ROW: u16 = 2;

/// Columns go from 0 to 59 (12px per col, so 720px)
/// Rows go from 0 to 200 (2px per row, so 400px)
struct AddrWindow {
    col_start: u16,
    col_end: u16,
    row_start: u16,
    row_end: u16,
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HpmFps {
    Sixteen = 0b00000000,
    ThirtyTwo = 0b00010000,
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LpmFps {
    Quarter = 0b000,
    Half = 0b001,
    One = 0b010,
    Two = 0b011,
    Four = 0b100,
    Eight = 0b101,
}

pub struct FpsConfig {
    pub hpm: HpmFps,
    pub lpm: LpmFps,
}

impl FpsConfig {
    pub fn as_u8(&self) -> u8 {
        (self.hpm as u8) + (self.lpm as u8)
    }
}

/// ST7306 driver to connect to TFT displays.
pub struct ST7306<SPI, DC, CS, RST, const COLS: usize, const ROWS: usize>
where
    SPI: spi::Write<u8>,
    DC: OutputPin,
    CS: OutputPin,
    RST: OutputPin,
{
    /// SPI
    pub spi: SPI,

    /// Data/command pin.
    pub dc: DC,

    /// Chip select pin
    pub cs: CS,

    /// Reset pin.
    pub rst: RST,

    /// Whether the colours are inverted (true) or not (false)
    inverted: bool,

    framebuffer: [[[u8; 3]; COLS]; ROWS],

    /// Auto power down
    autopowerdown: bool,

    /// Enable tearing pin
    te_enable: bool,

    fps: FpsConfig,

    /// Global image offset
    dx: u16,
    dy: u16,
    width: u16,
    height: u16,
    addr_window: AddrWindow,

    sleeping: bool,
    power_mode: PowerMode,
    display_on: bool,
}

#[derive(Clone, Copy)]
pub enum Orientation {
    Portrait = 0x00,
    Landscape = 0x60,
    PortraitSwapped = 0xC0,
    LandscapeSwapped = 0xA0,
}

impl<SPI, DC, CS, RST, const COLS: usize, const ROWS: usize> ST7306<SPI, DC, CS, RST, COLS, ROWS>
where
    SPI: spi::Write<u8>,
    DC: OutputPin,
    CS: OutputPin,
    RST: OutputPin,
{
    /// Creates a new driver instance that uses hardware SPI.
    pub fn new(
        spi: SPI,
        dc: DC,
        cs: CS,
        rst: RST,
        inverted: bool,
        autopowerdown: bool,
        te_enable: bool,
        fps: FpsConfig,
        width: u16,
        height: u16,
        col_start: u16,
        row_start: u16,
    ) -> Self {
        // TODO: This might be incorrect, if the pixels don't fit exactly into cols and rows
        // 0 indexed
        let col_end = col_start + (width / PX_PER_COL) - 1;
        let row_end = row_start + (height / PX_PER_ROW) - 1;
        assert!(col_end < COL_MAX);
        assert!(row_end < ROW_MAX);

        // TODO: Remove this
        assert_eq!(col_end, 0x2A);
        assert_eq!(row_end, 0xC7);

        let addr_window = AddrWindow {
            col_start,
            col_end,
            row_start,
            row_end,
        };
        ST7306 {
            spi,
            dc,
            cs,
            rst,
            inverted,
            framebuffer: [[[0; 3]; COLS]; ROWS],
            fps,
            autopowerdown,
            te_enable,
            dx: 0,
            dy: 0,
            width,
            height,
            sleeping: true,
            power_mode: PowerMode::Hpm,
            display_on: false,
            addr_window,
        }
    }

    /// Draw individual pixels
    ///
    /// Since the display controller doesn't have a command to send individual
    /// pixels, we draw it to a framebuffer and then optionally flush all of
    /// that to the contoller.
    pub fn draw_pixels<I>(&mut self, pixels: I, flush: bool) -> Result<(), ()>
    where
        I: IntoIterator<Item = Pixel<Rgb565>>,
    {
        for Pixel(coord, color) in pixels.into_iter() {
            // Only draw pixels that would be on screen
            if coord.x >= 0
                && coord.y >= 0
                && coord.x < self.width as i32
                && coord.y < self.height as i32
            {
                self.set_pixel(
                    coord.x as u16,
                    coord.y as u16,
                    RawU16::from(color).into_inner() as u8,
                )?;
            }
        }
        if flush {
            self.flush()?;
        }
        Ok(())
    }

    /// Flush the entire framebuffer to the screen
    ///
    /// TODO: Support partial screen updates
    ///       Need to keep track of which cols and rows have changed.
    pub fn flush(&mut self) -> Result<(), ()> {
        let caset = (18, (42));
        let raset = (0, (199));
        self.write_command(Instruction::CASET, &[caset.0, caset.1])?;
        self.write_command(Instruction::RASET, &[raset.0, raset.1])?;

        self.write_command(Instruction::RAMWR, &[])?;
        self.start_data()?;

        for row in 0..ROWS {
            for col in 0..COLS {
                self.write_byte(self.framebuffer[row][col][0])?;
                self.write_byte(self.framebuffer[row][col][1])?;
                self.write_byte(self.framebuffer[row][col][2])?;
            }
        }
        Ok(())
    }

    // TODO: Can implement
    //pub fn fill_contiguous_single_color(
    //    &mut self,
    //    area: &Rectangle,
    //    color: Rgb565,
    //) -> Result<(), ()> {
    //    // Clamp area to drawable part of the display target
    //    let drawable_area = area.intersection(&Rectangle::new(Point::zero(), self.size()));
    //    let brightness = col_to_bright(color);
    //    let colors =
    //        core::iter::repeat(brightness).take((area.size.width * area.size.height) as usize);
    //    //let colors = area.points()
    //    //            .filter(|pos| drawable_area.contains(*pos))
    //    //            .map(|_pos| brightness);

    //    if drawable_area.size != Size::zero() {
    //        let ex = (drawable_area.top_left.x + (drawable_area.size.width - 1) as i32) as u16;
    //        let ey = (drawable_area.top_left.y + (drawable_area.size.height - 1) as i32) as u16;
    //        self.set_pixels_buffered_u8(
    //            drawable_area.top_left.x as u16,
    //            drawable_area.top_left.y as u16,
    //            ex,
    //            ey,
    //            colors,
    //        )?;
    //    }

    //    Ok(())
    //}

    /// Runs commands to initialize the display.
    pub fn init<DELAY>(&mut self, delay: &mut DELAY) -> Result<(), ()>
    where
        DELAY: DelayMs<u8>,
    {
        // First do a hard reset because the controller might be in a bad state
        // if the voltage was unstable in the beginning.
        self.hard_reset(delay)?;
        self.write_command(Instruction::SWRESET, &[])?;
        delay.delay_ms(200);

        // 0x17 = 10111 VS_EN=1, ID_EN=1 (both off would be 0b10001)
        // 0x02 = 00010 V  NVM Load by timer=0, load by slpout=1 (both off would be 0b0)
        //self.write_command(Instruction::NVMLOADCTRL, &[0x17, 0x02])?;
        self.write_command(Instruction::NVMLOADCTRL, &[0b10001, 0])?;
        self.write_command(Instruction::BSTEN, &[0x01])?;

        // Gate Voltage Control. VGH: 12V, VGL: -6V
        self.write_command(Instruction::GCTRL, &[0x08, 0x02])?;
        // VSHP Control: 4.02V
        self.write_command(Instruction::VSHPCTRL, &[0x0B, 0x0B, 0x0B, 0x0B])?;
        // VSLP Control: 0.8V
        self.write_command(Instruction::VSLPCTRL, &[0x23, 0x23, 0x23, 0x23])?;
        // VSHN Control: -3.28V
        self.write_command(Instruction::VSHNCTRL, &[0x27, 0x27, 0x27, 0x27])?;
        // VSLN Control: -0.06V
        self.write_command(Instruction::VSLNCTRL, &[0x35, 0x35, 0x35, 0x35])?;

        // Datasheet: 0x32, 0x03, 0x1F Reference code: not present
        //self.write_command(Instruction::GTCON, &[0x32, 0x03, 0x1F])?;

        // Datasheet: 0x26, 0xE9, Reference: 0xA6, 0xE9 (HPM: 32Hz)
        self.write_command(Instruction::OSCSET, &[0xA6, 0xE9])?;

        // Frame Rate Control: 32Hz in High Power Mode, 1Hz in Low Power Mode
        // Examples
        // 0x12 = 0b10010 (32Hz in HPM, 1Hz in LPM)
        // 0x15 = 0b10101 (32Hz in HPM, 8Hz in LPM)
        self.write_command(Instruction::FRCTRL, &[self.fps.as_u8()])?;

        // HPM EQ Control
        self.write_command(
            Instruction::GTUPEQH,
            &[0xE5, 0xF6, 0x05, 0x46, 0x77, 0x77, 0x77, 0x77, 0x76, 0x45],
        )?;
        // LPM EQ Control
        self.write_command(
            Instruction::GTUPEQL,
            &[0x05, 0x46, 0x77, 0x77, 0x77, 0x77, 0x76, 0x45],
        )?;
        // Source EQ Enable
        self.write_command(Instruction::SOUEQ, &[0x13])?;

        // Gate Line Setting:
        // 0x64 (100) lines. Each line controls 2 pixels. 100*2 = 400px
        self.write_command(Instruction::GATESET, &[0x64])?;

        // Exit sleep mode
        self.write_command(Instruction::SLPOUT, &[])?;
        self.sleeping = false;
        delay.delay_ms(255);

        // Ultra low power code (undocumented command)
        self.write_command(Instruction::LOWPOWER, &[0xC1, 0x4A, 0x26])?;

        // Source Voltage Select: VSHP1, VSLP1, VSHN1, VSLN1
        self.write_command(Instruction::VSHLSEL, &[0x00])?;

        // Memory Data Access Control. Default, nothing inverted
        //                 0      = MY (Page Address Order) Flips picture upside down
        //                  1     = MX (Column Address Order)
        //                   0    = MV (Page/Column Order)
        //                     1  = DO (Data Order)
        //                      0 = GS (Gate Scan Order)
        //                 010010
        // Make sure pixel 0,0 is in the top left
        let madctl: u8 = 0b01001000;
        self.write_command(Instruction::MADCTL, &[madctl])?;

        // Data Format: XDE=1, BPS=1 (3 bytes for 24 bits)
        self.write_command(Instruction::DTFORM, &[0x11])?;

        // Gamma Mode: Mono
        self.write_command(Instruction::GAMAMS, &[0x20])?;

        // Panel Setting
        //  01      = 1-Dot Inversion
        //  || 10   = Frame Interval
        //  || ||01 = One-Line Interface
        //  || ||||
        // 00101001 = 0x29
        self.write_command(Instruction::PNLSET, &[0x29])?;

        // Column and row settings.
        // Will be overridden by each pixel write
        // Columns 18-42 (S217-S516). 25 columns, one for 12 pixels => 300px
        self.write_command(
            Instruction::CASET,
            &[
                self.addr_window.col_start as u8,
                self.addr_window.col_end as u8,
            ],
        )?;
        // Rows 0-199 (G1-G402). 200 rows, one for 2 pixels => 400px
        self.write_command(
            Instruction::RASET,
            &[
                self.addr_window.row_start as u8,
                self.addr_window.row_end as u8,
            ],
        )?;

        // Enable auto power down
        if self.autopowerdown {
            self.write_command(Instruction::AUTOPWRCTRL, &[0xFF])?;
        } else {
            self.write_command(Instruction::AUTOPWRCTRL, &[0x7F])?;
        }

        // Tearing enable on
        if self.te_enable {
            self.write_command(Instruction::TEON, &[])?;
        } else {
            self.write_command(Instruction::TEOFF, &[])?;
        }

        // Go into low power mode
        //self.write_command(Instruction::LPM, &[])?;
        self.write_command(Instruction::LPM, &[])?;
        self.power_mode = PowerMode::Lpm;

        // Invert screen colors
        self.invert_screen(self.inverted)?;

        self.on_off(true)?;

        Ok(())
    }

    pub fn on_off(&mut self, on: bool) -> Result<(), ()> {
        if on {
            self.write_command(Instruction::DISPON, &[])?;
        } else {
            self.write_command(Instruction::DISPOFF, &[])?;
        }
        self.display_on = on;
        Ok(())
    }

    pub fn sleep_in<DELAY>(&mut self, delay: &mut DELAY) -> Result<(), ()>
    where
        DELAY: DelayMs<u8>,
    {
        match self.power_mode {
            PowerMode::Hpm => {
                self.write_command(Instruction::SLPIN, &[])?;
                delay.delay_ms(100);
            }
            PowerMode::Lpm => {
                self.switch_mode(delay, PowerMode::Hpm)?;
                delay.delay_ms(255);
                self.sleep_in(delay)?;
            }
        }
        self.sleeping = true;
        Ok(())
    }

    pub fn sleep_out<DELAY>(&mut self, delay: &mut DELAY) -> Result<(), ()>
    where
        DELAY: DelayMs<u8>,
    {
        self.write_command(Instruction::SLPOUT, &[])?;
        delay.delay_ms(100);
        self.sleeping = false;
        Ok(())
    }

    pub fn switch_mode<DELAY>(
        &mut self,
        delay: &mut DELAY,
        target_mode: PowerMode,
    ) -> Result<(), ()>
    where
        DELAY: DelayMs<u8>,
    {
        if target_mode == self.power_mode {
            return Ok(());
        }
        match target_mode {
            PowerMode::Hpm => {
                self.write_command(Instruction::HPM, &[])?;
                delay.delay_ms(255);
            }
            PowerMode::Lpm => {
                self.write_command(Instruction::LPM, &[])?;
                delay.delay_ms(100);
            }
        }
        self.power_mode = target_mode;
        Ok(())
    }

    pub fn invert_screen(&mut self, inverted: bool) -> Result<(), ()> {
        if inverted {
            self.write_command(Instruction::INVON, &[])?;
        } else {
            self.write_command(Instruction::INVOFF, &[])?;
        }
        self.inverted = inverted;
        Ok(())
    }

    pub fn hard_reset<DELAY>(&mut self, delay: &mut DELAY) -> Result<(), ()>
    where
        DELAY: DelayMs<u8>,
    {
        self.rst.set_high().map_err(|_| ())?;
        delay.delay_ms(10);

        self.rst.set_low().map_err(|_| ())?;
        delay.delay_ms(10);

        self.rst.set_high().map_err(|_| ())
    }

    pub fn write_command(&mut self, command: Instruction, params: &[u8]) -> Result<(), ()> {
        self.cs.set_low().map_err(|_| ())?;
        self.dc.set_low().map_err(|_| ())?;
        self.spi.write(&[command as u8]).map_err(|_| ())?;
        if !params.is_empty() {
            self.start_data()?;
            self.write_data(params)?;
        }
        self.cs.set_high().map_err(|_| ())?;
        Ok(())
    }

    pub fn start_data(&mut self) -> Result<(), ()> {
        self.cs.set_low().map_err(|_| ())?;
        self.dc.set_high().map_err(|_| ())
    }

    pub fn write_data(&mut self, data: &[u8]) -> Result<(), ()> {
        data.iter().fold(Ok(()), |res, byte| {
            self.spi.write(&[*byte as u8]).map_err(|_| ())?;
            res
        })
    }

    pub fn write_ram(&mut self, data: &[(u8, u8, u8)]) -> Result<(), ()> {
        data.iter().fold(Ok(()), |res, (first, second, third)| {
            self.spi.write(&[*first as u8]).map_err(|_| ())?;
            self.spi.write(&[*second as u8]).map_err(|_| ())?;
            self.spi.write(&[*third as u8]).map_err(|_| ())?;
            res
        })
    }

    pub fn write_byte(&mut self, value: u8) -> Result<(), ()> {
        self.write_data(&[value])
    }

    pub fn set_orientation(&mut self, _orientation: &Orientation) -> Result<(), ()> {
        panic!("TODO: Not yet implemented");
        //self.write_command(Instruction::MADCTL, &[*orientation as u8])?;
        //Ok(())
    }

    /// Sets the global offset of the displayed image
    pub fn set_offset(&mut self, dx: u16, dy: u16) {
        self.dx = dx;
        self.dy = dy;
    }

    /// Sets the address window for the display.
    pub fn set_address_window(&mut self, sx: u16, sy: u16, ex: u16, ey: u16) -> Result<(), ()> {
        let x_lower = self.addr_window.col_start + (sx + self.dx) / PX_PER_COL;
        let x_upper = self.addr_window.col_end + (ex + self.dx) / PX_PER_COL;
        let y_lower = self.addr_window.row_start + (sy + self.dy) / PX_PER_ROW;
        let y_upper = self.addr_window.row_end + (ey + self.dy) / PX_PER_ROW;
        self.write_command(Instruction::CASET, &[x_lower as u8, x_upper as u8])?;
        self.write_command(Instruction::RASET, &[y_lower as u8, y_upper as u8])?;
        Ok(())
    }

    /// Sets a pixel color at the given coords.
    ///
    /// Changes the pixel value in the framebuffer at the bit where the
    /// display controller expects it.
    pub fn set_pixel(&mut self, x: u16, y: u16, color: u8) -> Result<(), ()> {
        let row = (y / PX_PER_ROW) as usize;
        let col = (x / PX_PER_COL) as usize;
        let black = color < 1;

        let (byte, bitmask) = match (x % PX_PER_COL, y % PX_PER_ROW) {
            (0, 0) => (0, 0x80),
            (0, 1) => (0, 0x40),
            (1, 0) => (0, 0x20),
            (1, 1) => (0, 0x10),
            (2, 0) => (0, 0x08),
            (2, 1) => (0, 0x04),
            (3, 0) => (0, 0x02),
            (3, 1) => (0, 0x01),

            (4, 0) => (1, 0x80),
            (4, 1) => (1, 0x40),
            (5, 0) => (1, 0x20),
            (5, 1) => (1, 0x10),
            (6, 0) => (1, 0x08),
            (6, 1) => (1, 0x04),
            (7, 0) => (1, 0x02),
            (7, 1) => (1, 0x01),

            (8, 0) => (2, 0x80),
            (8, 1) => (2, 0x40),
            (9, 0) => (2, 0x20),
            (9, 1) => (2, 0x10),
            (10, 0) => (2, 0x08),
            (10, 1) => (2, 0x04),
            (11, 0) => (2, 0x02),
            (11, 1) => (2, 0x01),
            _ => panic!("Impossible to reach"),
        };

        if black {
            self.framebuffer[row][col][byte] |= bitmask
        } else {
            self.framebuffer[row][col][byte] &= !bitmask;
        }
        Ok(())
    }

    /// Writes pixel colors sequentially into the current drawing window
    ///
    /// Must fill out the window, or at least write in an amount of pixels
    /// divisible by 24.
    pub fn write_pixels<P: IntoIterator<Item = u8>>(&mut self, colors: P) -> Result<(), ()> {
        // TODO: Check if same
        // Only works if writing all pixels
        self.write_command(Instruction::RAMWR, &[])?;
        self.start_data()?;
        for color in colors {
            self.write_byte(color)?;
        }
        Ok(())
    }
}

#[cfg(feature = "graphics")]
extern crate embedded_graphics;
#[cfg(feature = "graphics")]
use self::embedded_graphics::{
    draw_target::DrawTarget,
    pixelcolor::{
        raw::{RawData, RawU16},
        Rgb565,
    },
    prelude::*,
};

fn col_to_bright(color: Rgb565) -> u8 {
    ((color.r() as u16) + (color.g() as u16) + (color.b() as u16) / 3) as u8
}

#[cfg(feature = "graphics")]
// TODO: Remove color support from here
impl<SPI, DC, CS, RST, const COLS: usize, const ROWS: usize> DrawTarget
    for ST7306<SPI, DC, CS, RST, COLS, ROWS>
where
    SPI: spi::Write<u8>,
    DC: OutputPin,
    CS: OutputPin,
    RST: OutputPin,
{
    type Error = ();
    type Color = Rgb565;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        self.draw_pixels(pixels, true)
    }

    //fn fill_contiguous<I>(&mut self, area: &Rectangle, colors: I) -> Result<(), Self::Error>
    //where
    //    I: IntoIterator<Item = Self::Color>,
    //{
    //    // Clamp area to drawable part of the display target
    //    let drawable_area = area.intersection(&Rectangle::new(Point::zero(), self.size()));
    //    let colors = area
    //        .points()
    //        .zip(colors)
    //        .filter(|(pos, _color)| drawable_area.contains(*pos))
    //        .map(|(_pos, color)| col_to_bright(color));
    //    //let colors =
    //    //        area.points()
    //    //            .zip(colors)
    //    //            .filter(|(pos, _color)| drawable_area.contains(*pos))
    //    //            .map(|(_pos, color)| RawU16::from(color).into_inner());

    //    if drawable_area.size != Size::zero() {
    //        let ex = (drawable_area.top_left.x + (drawable_area.size.width - 1) as i32) as u16;
    //        let ey = (drawable_area.top_left.y + (drawable_area.size.height - 1) as i32) as u16;
    //        self.set_pixels_buffered_u8(
    //            drawable_area.top_left.x as u16,
    //            drawable_area.top_left.y as u16,
    //            ex,
    //            ey,
    //            colors,
    //        )?;
    //    }

    //    Ok(())
    //}

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        let brightness = col_to_bright(color);
        let black = if brightness < 128 { 0xFF } else { 0x00 };

        for col in 0..COLS {
            for row in 0..ROWS {
                self.framebuffer[row][col][0] = black;
                self.framebuffer[row][col][1] = black;
                self.framebuffer[row][col][2] = black;
            }
        }
        self.flush()
    }
}

#[cfg(feature = "graphics")]
impl<SPI, DC, CS, RST, const COLS: usize, const ROWS: usize> OriginDimensions
    for ST7306<SPI, DC, CS, RST, COLS, ROWS>
where
    SPI: spi::Write<u8>,
    DC: OutputPin,
    CS: OutputPin,
    RST: OutputPin,
{
    fn size(&self) -> Size {
        Size::new(self.width as u32, self.height as u32)
    }
}
