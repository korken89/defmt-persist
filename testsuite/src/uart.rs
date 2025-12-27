//! Simple UART0 driver for LM3S6965 (QEMU testing only).
//!
//! This provides raw byte output via UART0, which QEMU connects to the serial port.

use core::ptr::{read_volatile, write_volatile};

const UART0_BASE: usize = 0x4000_C000;
const UART0_DR: *mut u32 = UART0_BASE as *mut u32; // Data Register
const UART0_FR: *const u32 = (UART0_BASE + 0x018) as *const u32; // Flag Register

const UART_FR_TXFF: u32 = 0x20; // Transmit FIFO Full

/// Write a single byte to UART0.
pub fn write_byte(byte: u8) {
    // Wait until TX FIFO is not full
    unsafe {
        while read_volatile(UART0_FR) & UART_FR_TXFF != 0 {}
        write_volatile(UART0_DR, byte as u32);
    }
}

/// Write a byte slice to UART0.
pub fn write_bytes(bytes: &[u8]) {
    for &byte in bytes {
        write_byte(byte);
    }
}
