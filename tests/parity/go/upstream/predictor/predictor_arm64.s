//go:build !purego

#include "textflag.h"

// PREFIX turns the 16 byte deltas of one channel in v into values:
// add lanes shifted up by 1, 2, 4, 8 (V31 is zero), then the carry
// c (last value, broadcast), and broadcast v's last lane to c.
#define PREFIX(v, c) \
	VEXT $15, v.B16, V31.B16, V30.B16 \
	VADD V30.B16, v.B16, v.B16 \
	VEXT $14, v.B16, V31.B16, V30.B16 \
	VADD V30.B16, v.B16, v.B16 \
	VEXT $12, v.B16, V31.B16, V30.B16 \
	VADD V30.B16, v.B16, v.B16 \
	VEXT $8, v.B16, V31.B16, V30.B16 \
	VADD V30.B16, v.B16, v.B16 \
	VADD c.B16, v.B16, v.B16 \
	VDUP v.B[15], c.B16

// func expandNEON(dst, src *byte, n, spp int, undo, inv bool)
// LD1/LD3/LD4 split channels, ST4 interleaves RGBA.
TEXT ·expandNEON(SB), NOSPLIT, $0-34
	MOVD  dst+0(FP), R0
	MOVD  src+8(FP), R1
	MOVD  n+16(FP), R2
	MOVD  spp+24(FP), R3
	MOVBU undo+32(FP), R4
	MOVBU inv+33(FP), R5
	VEOR  V31.B16, V31.B16, V31.B16
	VEOR  V16.B16, V16.B16, V16.B16
	VEOR  V17.B16, V17.B16, V17.B16
	VEOR  V18.B16, V18.B16, V18.B16
	VEOR  V5.B16, V5.B16, V5.B16
	CBZ   R5, noinv
	VMOVI $255, V5.B16 // 255-v as v^0xff

noinv:
	VMOVI $255, V3.B16
	CMP   $1, R3
	BEQ   grey
	CMP   $3, R3
	BEQ   rgb

rgbx:
	VLD4.P 64(R1), [V0.B16, V1.B16, V2.B16, V3.B16]
	VMOVI  $255, V3.B16
	CBZ    R4, rgbxout
	PREFIX(V0, V16)
	PREFIX(V1, V17)
	PREFIX(V2, V18)

rgbxout:
	VST4.P [V0.B16, V1.B16, V2.B16, V3.B16], 64(R0)
	SUBS   $1, R2, R2
	BNE    rgbx
	RET

rgb:
	VLD3.P 48(R1), [V0.B16, V1.B16, V2.B16]
	CBZ    R4, rgbout
	PREFIX(V0, V16)
	PREFIX(V1, V17)
	PREFIX(V2, V18)

rgbout:
	VST4.P [V0.B16, V1.B16, V2.B16, V3.B16], 64(R0)
	SUBS   $1, R2, R2
	BNE    rgb
	RET

grey:
	VLD1.P 16(R1), [V0.B16]
	CBZ    R4, greyout
	PREFIX(V0, V16)

greyout:
	VEOR   V5.B16, V0.B16, V0.B16
	VMOV   V0.B16, V1.B16
	VMOV   V0.B16, V2.B16
	VST4.P [V0.B16, V1.B16, V2.B16, V3.B16], 64(R0)
	SUBS   $1, R2, R2
	BNE    grey
	RET

// UNDO runs one group of undo8/undo16 on V0 (see undoTab), V15 carry.
// A is the lane arrangement for the adds.
#define UNDO(A) \
	VMOV V0.B16, V7.B16 \
	VTBL V8.B16, [V0.B16], V1.B16 \
	VADD V1.A, V0.A, V0.A \
	VTBL V9.B16, [V0.B16], V1.B16 \
	VADD V1.A, V0.A, V0.A \
	VTBL V10.B16, [V0.B16], V1.B16 \
	VADD V1.A, V0.A, V0.A \
	VTBL V11.B16, [V0.B16], V1.B16 \
	VADD V1.A, V0.A, V0.A \
	VADD V15.A, V0.A, V0.A \
	VTBL V12.B16, [V0.B16], V15.B16 \
	VBIF V13.B16, V7.B16, V0.B16

// func undo8(p *byte, n, step int, t *[6][16]byte)
// The next group loads before this one stores (they overlap
// when g < 16), so it sees its own input bytes.
TEXT ·undo8(SB), NOSPLIT, $0-32
	MOVD   p+0(FP), R0
	MOVD   n+8(FP), R1
	MOVD   step+16(FP), R2
	MOVD   t+24(FP), R3
	VLD1.P 64(R3), [V8.B16, V9.B16, V10.B16, V11.B16]
	VLD1   (R3), [V12.B16, V13.B16]
	VEOR   V15.B16, V15.B16, V15.B16
	VLD1   (R0), [V0.B16]

u8next:
	UNDO(B16)
	SUBS $1, R1, R1
	BEQ  u8last
	ADD  R2, R0, R4
	VLD1 (R4), [V6.B16]
	VST1 [V0.B16], (R0)
	MOVD R4, R0
	VMOV V6.B16, V0.B16
	B    u8next

u8last:
	VST1 [V0.B16], (R0)
	RET

// func undo16(p *byte, n, step int, t *[6][16]byte)
TEXT ·undo16(SB), NOSPLIT, $0-32
	MOVD   p+0(FP), R0
	MOVD   n+8(FP), R1
	MOVD   step+16(FP), R2
	MOVD   t+24(FP), R3
	VLD1.P 64(R3), [V8.B16, V9.B16, V10.B16, V11.B16]
	VLD1   (R3), [V12.B16, V13.B16]
	VEOR   V15.B16, V15.B16, V15.B16
	VLD1   (R0), [V0.B16]

u16next:
	UNDO(H8)
	SUBS $1, R1, R1
	BEQ  u16last
	ADD  R2, R0, R4
	VLD1 (R4), [V6.B16]
	VST1 [V0.B16], (R0)
	MOVD R4, R0
	VMOV V6.B16, V0.B16
	B    u16next

u16last:
	VST1 [V0.B16], (R0)
	RET
