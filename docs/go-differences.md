# Differences from Go

## Readers, predictor and errors


`deflate::Reader` is the raw one. A reader takes only the bytes it consumes from its input, so
after the end of the stream what follows (a PNG chunk) is still there. `deflate::predictor` has
`undo_expand_rgba`, `expand_rgba` and `undo_horizontal`, which write into the caller's buffers.

Errors are one enum, `deflate::Error`, with a variant for each Go error: `Corrupt(offset)` is
`flate.CorruptInputError`, `UnexpectedEof` is `io.ErrUnexpectedEOF`, `Header`, `Checksum` and
`Dictionary` are the `zlib.Err*` values, `Internal` is `flate.InternalError`, and `Io(kind)` is
an error from the caller's reader (its kind, not the value). A reader returns them as the source of
an `io::Error`.

## Behaviour that differs

Not the same: `decode`, `decode_limit` and `zlib::decode` return `Err` without the output before the error,
where Go returns both. `decode_into`, `decode_limit_into` and `zlib::decode_into` /
`decode_limit_into` leave those bytes in the caller's `Vec`, as does a reader before its error;
the `parity` job compares them for every operation (`partial` lines of the manifest). A reader's error
is final: `Error::Io(Interrupted)` from the input comes back as `io::ErrorKind::Other`, so
`read_to_end` does not retry forever. `read` with an empty buffer returns the stored
error once there is one, else `Ok(0)`, as Go's `Read(nil)`; Go also runs a decode step on a
`Read(nil)` that has no pending data (so it can return a new error or finish the stream), and this
crate does not.

## Not ported

Left scalar, though Go has asm for them: the predictor kernels (undo horizontal, RGBA expand).
They are not SIMD-ed because no benchmark here shows the gain. The SSSE3 adler32 tier is not
ported: AVX2 is the base tier, with AVX-512 VNNI above it.

See also the Go tests that are not applicable here in [parity.md](parity.md).
