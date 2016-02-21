mod components;
mod buffer;
mod square;
mod triangle;
mod noise;
mod dmc;

use super::memory::MemSegment;
use audio::AudioOut;
use std::cmp;
use cpu::IrqInterrupt;
use std::cell::RefCell;
use std::rc::Rc;
use apu::buffer::*;
use apu::square::*;
use apu::triangle::*;
use apu::noise::*;
use apu::dmc::*;

pub type Sample = i16;

static NTSC_TICK_LENGTH_TABLE: [[u64; 6]; 2] = [[7459, 7456, 7458, 7458, 7458, 0000],
                                                [0001, 7458, 7456, 7458, 7458, 7452]];

bitflags! {
    flags Frame : u8 {
        const MODE = 0b1000_0000, //0 = 4-step, 1 = 5-step
        const SUPPRESS_IRQ  = 0b0100_0000, //0 = disabled, 1 = enabled
    }
}

impl Frame {
    fn mode(&self) -> usize {
        if self.contains(MODE) {
            return 1;
        } else {
            return 0;
        }
    }
}

trait Writable {
    fn write(&mut self, idx: u16, val: u8);
}

enum Jitter {
    Delay(u64, u8),
    None,
}

pub struct APU {
    square1: Square,
    square2: Square,
    triangle: Triangle,
    noise: Noise,
    dmc: DMC,
    frame: Frame,
    
    square_buffer: Rc<RefCell<SampleBuffer>>,
    
    device: Box<AudioOut>,

    global_cyc: u64,
    tick: u8,
    next_tick_cyc: u64,
    next_transfer_cyc: u64,
    last_frame_cyc: u64,

    irq_requested: bool,

    jitter: Jitter,
}

impl APU {
    pub fn new(device: Box<AudioOut>) -> APU {
        let sample_rate = device.sample_rate();
        
        let square_buffer = Rc::new(RefCell::new(SampleBuffer::new(sample_rate)));
        let clocks_needed = square_buffer.borrow().clocks_needed() as u64;
        
        APU {
            square1: Square::new(false, Waveform::new(square_buffer.clone())),
            square2: Square::new(true, Waveform::new(square_buffer.clone())),
            triangle: Triangle::new(),
            noise: Noise::new(),
            dmc: DMC::new(),
            frame: Frame::empty(),

            square_buffer: square_buffer,

            device: device,

            global_cyc: 0,
            tick: 0,
            next_tick_cyc: NTSC_TICK_LENGTH_TABLE[0][0],
            next_transfer_cyc: clocks_needed,
            last_frame_cyc: 0,

            irq_requested: false,

            jitter: Jitter::None,
        }
    }

    pub fn run_to(&mut self, cpu_cycle: u64) -> IrqInterrupt {
        let mut interrupt = IrqInterrupt::None;

        while self.global_cyc < cpu_cycle {
            let current_cycle = self.global_cyc;

            let mut next_step = cmp::min(cpu_cycle, self.next_tick_cyc);
            next_step = cmp::min(next_step, self.next_transfer_cyc);

            if let Jitter::Delay(time, _) = self.jitter {
                next_step = cmp::min(next_step, time);
            }

            self.play(current_cycle, next_step);
            self.global_cyc = next_step;

            if let Jitter::Delay(time, val) = self.jitter {
                if self.global_cyc == time {
                    self.set_4017(val);
                    self.jitter = Jitter::None;
                }
            }
            if self.global_cyc == self.next_tick_cyc {
                interrupt = interrupt.or(self.tick());
            }
            if self.global_cyc == self.next_transfer_cyc {
                self.transfer();
            }
        }
        interrupt
    }

    /// Represents the 240Hz output of the frame sequencer's divider
    fn tick(&mut self) -> IrqInterrupt {
        self.tick += 1;
        let mode = self.frame.mode();
        self.next_tick_cyc = self.global_cyc + NTSC_TICK_LENGTH_TABLE[mode][self.tick as usize];

        match mode {
            0 => {
                match self.tick {
                    1 => {
                        self.envelope_tick();
                    }
                    2 => {
                        self.envelope_tick();
                        self.length_tick();
                    }
                    3 => {
                        self.envelope_tick();
                    }
                    4 => {
                        self.tick = 0;
                        self.envelope_tick();
                        self.length_tick();
                        return self.raise_irq();
                    }
                    _ => {
                        self.tick = 0;
                    }
                }
            }
            1 => {
                match self.tick {
                    1 => {
                        self.envelope_tick();
                        self.length_tick()
                    }
                    2 => {
                        self.envelope_tick();
                    }
                    3 => {
                        self.envelope_tick();
                        self.length_tick()
                    }
                    4 => {
                        self.envelope_tick();
                    }
                    5 => {
                        self.tick = 0;
                    } //4 is the actual last tick in the cycle.
                    _ => {
                        self.tick = 0;
                    }
                }
            }
            _ => (),
        }
        IrqInterrupt::None
    }

    fn envelope_tick(&mut self) {
        self.square1.envelope_tick();
        self.square2.envelope_tick();
        self.triangle.envelope_tick();
        self.noise.envelope_tick();
    }

    fn length_tick(&mut self) {
        self.square1.length_tick();
        self.square2.length_tick();
        self.triangle.length_tick();
        self.noise.length_tick();
    }

    fn raise_irq(&mut self) -> IrqInterrupt {
        if !self.frame.contains(SUPPRESS_IRQ) {
            self.irq_requested = true;
            return IrqInterrupt::IRQ;
        }
        return IrqInterrupt::None;
    }

    fn play(&mut self, from_cyc: u64, to_cyc: u64) {
        let from = (from_cyc - self.last_frame_cyc) as u32;
        let to = (to_cyc - self.last_frame_cyc) as u32;
        self.square1.play(from, to);
        self.square2.play(from, to);
        self.triangle.play(from, to);
        self.noise.play(from, to);
        self.dmc.play(from, to);
    }

    fn transfer(&mut self) {
        let cpu_cyc = self.global_cyc;
        let cycles_since_last_frame = (cpu_cyc - self.last_frame_cyc) as u32;
        self.last_frame_cyc = cpu_cyc;
        
        let mut square_buf = self.square_buffer.borrow_mut(); 
        square_buf.end_frame(cycles_since_last_frame);
        let samples: Vec<Sample> = {
            let iter1 = square_buf.read().iter();
            iter1.cloned().collect()
        };
        self.next_transfer_cyc = cpu_cyc + square_buf.clocks_needed() as u64;
        self.device.play(&samples);
    }

    ///Returns the cycle number representing the next time the CPU should run the APU.
    ///Min of the next APU IRQ, the next DMC IRQ, and the next tick time. When the CPU cycle reaches
    ///this number, the CPU must run the APU.
    pub fn requested_run_cycle(&self) -> u64 {
        // In practice, the next tick time should cover the APU IRQ as well, since the
        // IRQ happens on tick boundaries. The DMC IRQ isn't implemented yet.
        // Using the tick time ensures that the APU will never get too far behind the
        // CPU.
        self.next_tick_cyc
    }

    fn set_4017(&mut self, val: u8) {
        self.frame = Frame::from_bits_truncate(val);
        if self.frame.contains(SUPPRESS_IRQ) {
            self.irq_requested = false;
        }

        self.tick = 0;
        self.next_tick_cyc = self.global_cyc + NTSC_TICK_LENGTH_TABLE[self.frame.mode()][0];
    }

    #[cfg_attr(rustfmt, rustfmt_skip)]
    pub fn read_status(&mut self, cycle: u64) -> (IrqInterrupt, u8) {
        let interrupt = self.run_to(cycle - 1);

        let mut status: u8 = 0;
        status = status | (self.square1.length.active() << 0);
        status = status | (self.square2.length.active() << 1);
        status = status | (self.triangle.length.active() << 2);
        status = status | (self.noise.length.active() << 3);
        status = status | if self.irq_requested { 1 << 6 } else { 0 };
    // TODO add DMC status
    // TODO add DMC interrupt flag
        self.irq_requested = false;

        (interrupt.or(self.run_to(cycle)), status)
    }

    pub fn write(&mut self, idx: u16, val: u8) {
        match idx % 0x20 {
            x @ 0x00...0x03 => self.square1.write(x, val),
            x @ 0x04...0x07 => self.square2.write(x, val),
            x @ 0x08...0x0B => self.triangle.write(x, val),
            x @ 0x0C...0x0F => self.noise.write(x, val),
            x @ 0x10...0x13 => self.dmc.write(x, val),
            0x0014 => (),
            0x0015 => {
                self.noise.length.set_enable(val & 0b0000_1000 != 0);
                self.triangle.length.set_enable(val & 0b0000_0100 != 0);
                self.square2.length.set_enable(val & 0b0000_0010 != 0);
                self.square1.length.set_enable(val & 0b0000_0001 != 0);
            }
            0x0016 => (),
            0x0017 => {
                if self.global_cyc % 2 == 0 {
                    self.set_4017(val);
                } else {
                    self.jitter = Jitter::Delay(self.global_cyc + 1, val);
                }
            }
            _ => (),
        }
    }
}
