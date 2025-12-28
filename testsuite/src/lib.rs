#![no_std]

pub mod uart;

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use cortex_m_semihosting::debug::{self, EXIT_FAILURE, EXIT_SUCCESS};
use defmt_persist as _;

pub use cortex_m_rt::entry;

pub fn exit_success() -> ! {
    debug::exit(EXIT_SUCCESS);
    #[allow(clippy::empty_loop)]
    loop {}
}

pub fn exit_failure() -> ! {
    debug::exit(EXIT_FAILURE);
    #[allow(clippy::empty_loop)]
    loop {}
}

/// Dump the PERSIST region via UART1.
///
/// This outputs the raw bytes of the persist region via UART1,
/// which can be captured and used to initialize the region in
/// a subsequent test.
pub fn dump_persist_region() {
    unsafe extern "C" {
        static __defmt_persist_start: u8;
        static __defmt_persist_end: u8;
    }

    let start = &raw const __defmt_persist_start;
    let end = &raw const __defmt_persist_end;
    let len = end as usize - start as usize;
    let persist_data = unsafe { core::slice::from_raw_parts(start, len) };
    uart::write_bytes_uart1(persist_data);
}

/// Dump the PERSIST region via UART1 and exit successfully.
pub fn dump_persist_region_and_exit() -> ! {
    dump_persist_region();
    exit_success();
}

/// Yield once to allow other tasks to run.
pub async fn yield_once() {
    let mut yielded = false;
    core::future::poll_fn(|_cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            Poll::Pending
        }
    })
    .await
}

/// Minimal block_on executor for testing.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);

    // Create a no-op waker.
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    let raw_waker = RawWaker::new(core::ptr::null(), &VTABLE);
    let waker = unsafe { Waker::from_raw(raw_waker) };
    let mut cx = Context::from_waker(&waker);

    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(val) => return val,
            Poll::Pending => {
                cortex_m::asm::nop();
            }
        }
    }
}

/// Join two futures, polling them alternately until both complete.
pub async fn join<A, B, T, U>(a: A, b: B) -> (T, U)
where
    A: Future<Output = T>,
    B: Future<Output = U>,
{
    let mut a = pin!(a);
    let mut b = pin!(b);
    let mut a_done: Option<T> = None;
    let mut b_done: Option<U> = None;

    core::future::poll_fn(|cx| {
        if a_done.is_none() {
            if let Poll::Ready(val) = a.as_mut().poll(cx) {
                a_done = Some(val);
            }
        }
        if b_done.is_none() {
            if let Poll::Ready(val) = b.as_mut().poll(cx) {
                b_done = Some(val);
            }
        }
        if a_done.is_some() && b_done.is_some() {
            Poll::Ready((a_done.take().unwrap(), b_done.take().unwrap()))
        } else {
            Poll::Pending
        }
    })
    .await
}
