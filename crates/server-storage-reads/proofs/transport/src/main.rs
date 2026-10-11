//! Dependency build proof only: no sockets, adapter, keys or remote data.
fn main() {
    println!("private dependency graph only; no transport adapter");
}

#[cfg(test)]
mod tests {
    use brotli_decompressor::{BrotliDecompressStream, BrotliResult, BrotliState, StandardAlloc};
    #[test]
    fn br_large_window_refusal() {
        // Source DecodeWindowBits: 0x11 is the nonstandard large-window marker.
        // This fixed synthetic input is refused before ring allocation. The
        // ordinary constructor is deliberately not used for hostile allocation.
        let mut state = BrotliState::new_strict(
            StandardAlloc::default(),
            StandardAlloc::default(),
            StandardAlloc::default(),
        );
        let input = [0x11, 0x1e, 0, 0];
        let mut available_in = input.len();
        let mut offset_in = 0;
        let mut output = [0u8; 4096];
        let mut available_out = output.len();
        let mut offset_out = 0;
        let mut total_out = 0;
        let result = BrotliDecompressStream(
            &mut available_in,
            &mut offset_in,
            &input,
            &mut available_out,
            &mut offset_out,
            &mut output,
            &mut total_out,
            &mut state,
        );
        assert!(matches!(result, BrotliResult::ResultFailure));
        assert_eq!(total_out, 0);
        assert_eq!(state.ringbuffer_size, 0);
    }
}
