#![no_main]
use libfuzzer_sys::fuzz_target;
use powdb_server::protocol::Message;

// The server wire decoder is the top untrusted network surface: every byte a
// remote (pre-auth!) client sends flows through `Message::decode` and the
// framed readers. Decoding arbitrary bytes must NEVER panic, read out of
// bounds, or allocate proportionally to an attacker-declared count (the
// decoder carries payload-derived caps; this target proves them under the
// libFuzzer rss limit).
fuzz_target!(|data: &[u8]| {
    // 1. The raw frame decoder is total: Ok or Err, never a panic.
    if let Ok(message) = Message::decode(data) {
        // 2. Anything the decoder accepts must survive a re-encode/re-decode
        //    round trip (the encoder and decoder agree on the wire format).
        let bytes = message.encode();
        Message::decode(&bytes).expect("re-encoded accepted message must decode");
    }

    // 3. The framed async readers are equally total over the same bytes, for
    //    both the general path and the pre-auth CONNECT path (which carries
    //    a much smaller payload cap). `&[u8]` readers never return Pending,
    //    so a noop-waker poll loop is a complete executor here.
    block_on(async {
        let mut cursor: &[u8] = data;
        let _ = Message::read_from(&mut cursor).await;
        let mut cursor: &[u8] = data;
        let _ = Message::read_from_preauth(&mut cursor).await;
    });
});

/// Minimal block_on for futures that never return Pending (in-memory I/O).
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    fn noop_raw_waker() -> RawWaker {
        fn clone(_: *const ()) -> RawWaker {
            noop_raw_waker()
        }
        fn noop(_: *const ()) {}
        RawWaker::new(
            std::ptr::null(),
            &RawWakerVTable::new(clone, noop, noop, noop),
        )
    }

    let waker = unsafe { Waker::from_raw(noop_raw_waker()) };
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => unreachable!("in-memory reads never pend"),
        }
    }
}
