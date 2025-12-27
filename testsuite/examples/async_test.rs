//! Test for the async-await API.
//!
//! This test verifies that `Consumer::wait_for_data()` works correctly
//! by running a writer and reader task concurrently using join.

#![no_std]
#![no_main]

use core::future::Future;
use core::pin::{Pin, pin};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use defmt_persist::Consumer;
use testsuite::{entry, exit_success, uart};

/// Join two futures, polling them alternately until both complete.
async fn join<A, B, T, U>(a: A, b: B) -> (T, U)
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

/// Async task that writes log messages with yields in between.
async fn writer_task() {
    // Yield to let reader start waiting
    yield_once().await;

    defmt::info!("async test: message 1");
    yield_once().await;

    defmt::info!("async test: message 2");
    yield_once().await;

    defmt::info!("async test: message 3");
}

/// Async task that waits for data and reads it.
async fn reader_task(consumer: &mut Consumer<'static>) {
    for _ in 0..3 {
        // Wait for data to be available
        consumer.wait_for_data().await;

        // Read and output all available data
        loop {
            let data = consumer.read();
            if data.buf().is_empty() {
                break;
            }
            uart::write_bytes(data.buf());
            data.release(0xffffffff);
        }
    }
}

/// Yield once to allow other tasks to run.
async fn yield_once() {
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
fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);

    // Create a no-op waker
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE), // clone
        |_| {},                                        // wake
        |_| {},                                        // wake_by_ref
        |_| {},                                        // drop
    );
    let raw_waker = RawWaker::new(core::ptr::null(), &VTABLE);
    let waker = unsafe { Waker::from_raw(raw_waker) };
    let mut cx = Context::from_waker(&waker);

    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(val) => return val,
            Poll::Pending => {
                // In a real executor we'd wait for a wakeup,
                // but for this test we just spin
                cortex_m::asm::nop();
            }
        }
    }
}

#[entry]
fn main() -> ! {
    let Some(mut consumer) = defmt_persist::init() else {
        panic!("defmt-persist already initialized (or failed)");
    };

    // Run writer and reader concurrently
    block_on(join(writer_task(), reader_task(&mut consumer)));

    exit_success();
}
