//! Simple UART driver for LM3S6965 (QEMU testing only).
//!
//! Provides raw byte output via UART0 and UART1.
//! - UART0: defmt ring buffer output
//! - UART1: persist region dump
//!
//! QEMU serial port mapping: `qemu-system-arm ... -serial <uart0> -serial <uart1>`
//! The first `-serial` argument maps to UART0, the second to UART1, etc.

use core::ptr::{read_volatile, write_volatile};

const UART0_BASE: usize = 0x4000_C000;
const UART1_BASE: usize = 0x4000_D000;

const UART_DR_OFFSET: usize = 0x000; // Data Register
const UART_FR_OFFSET: usize = 0x018; // Flag Register
const UART_FR_TXFF: u32 = 0x20; // Transmit FIFO Full

/// Write a single byte to a UART.
fn uart_write_byte(base: usize, byte: u8) {
    let dr = (base + UART_DR_OFFSET) as *mut u32;
    let fr = (base + UART_FR_OFFSET) as *const u32;
    unsafe {
        while read_volatile(fr) & UART_FR_TXFF != 0 {}
        write_volatile(dr, byte as u32);
    }
}

/// Write a single byte to UART0.
pub fn write_byte(byte: u8) {
    uart_write_byte(UART0_BASE, byte);
}

/// Write a byte slice to UART0.
pub fn write_bytes(bytes: &[u8]) {
    for &byte in bytes {
        write_byte(byte);
    }
}

/// Write a single byte to UART1.
pub fn write_byte_uart1(byte: u8) {
    uart_write_byte(UART1_BASE, byte);
}

/// Write a byte slice to UART1.
pub fn write_bytes_uart1(bytes: &[u8]) {
    for &byte in bytes {
        write_byte_uart1(byte);
    }
}
