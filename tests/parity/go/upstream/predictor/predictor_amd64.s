//go:build !purego

#include "textflag.h"

// func greySSSE3(dst, src *byte, n int, undo, inv bool, m *[7][16]byte)
// X15 is the carry: the last decoded grey value in every lane.
TEXT ·greySSSE3(SB), NOSPLIT, $0-40
	MOVQ    dst+0(FP), DI
	MOVQ    src+8(FP), SI
	MOVQ    n+16(FP), CX
	MOVBLZX undo+24(FP), R8
	MOVBLZX inv+25(FP), R9
	MOVQ    m+32(FP), AX
	MOVOU   0(AX), X8
	MOVOU   16(AX), X9
	MOVOU   32(AX), X10
	MOVOU   48(AX), X11
	MOVOU   64(AX), X12
	PCMPEQB X13, X13
	PSLLL   $24, X13     // alpha
	PXOR    X14, X14
	TESTQ   R9, R9
	JZ      greyloop
	PCMPEQB X14, X14
	PSRLL   $8, X14      // 255-v as v^0xff

greyloop:
	PXOR  X15, X15

greynext:
	MOVOU (SI), X0
	TESTQ R8, R8
	JZ    greyout
	MOVO  X0, X1
	PSLLO $1, X1
	PADDB X1, X0
	MOVO  X0, X1
	PSLLO $2, X1
	PADDB X1, X0
	MOVO  X0, X1
	PSLLO $4, X1
	PADDB X1, X0
	MOVO  X0, X1
	PSLLO $8, X1
	PADDB X1, X0
	PADDB X15, X0
	MOVO  X0, X15
	PSHUFB X12, X15

greyout:
	MOVO   X0, X1
	PSHUFB X8, X1
	PXOR   X14, X1
	POR    X13, X1
	MOVOU  X1, 0(DI)
	MOVO   X0, X1
	PSHUFB X9, X1
	PXOR   X14, X1
	POR    X13, X1
	MOVOU  X1, 16(DI)
	MOVO   X0, X1
	PSHUFB X10, X1
	PXOR   X14, X1
	POR    X13, X1
	MOVOU  X1, 32(DI)
	MOVO   X0, X1
	PSHUFB X11, X1
	PXOR   X14, X1
	POR    X13, X1
	MOVOU  X1, 48(DI)
	ADDQ   $16, SI
	ADDQ   $64, DI
	DECQ   CX
	JNZ    greynext
	RET

// func rgbSSSE3(dst, src *byte, n, stride int, undo bool, shuf *[16]byte)
// Pixels sit in dword lanes with A=0, so PADDB keeps channels apart.
// X15 is the carry: the last decoded pixel in every lane.
TEXT ·rgbSSSE3(SB), NOSPLIT, $0-48
	MOVQ    dst+0(FP), DI
	MOVQ    src+8(FP), SI
	MOVQ    n+16(FP), CX
	MOVQ    stride+24(FP), DX
	MOVBLZX undo+32(FP), R8
	MOVQ    shuf+40(FP), AX
	MOVOU   (AX), X8
	PCMPEQB X13, X13
	PSLLL   $24, X13     // alpha
	PXOR    X15, X15

rgbnext:
	MOVOU  (SI), X0
	PSHUFB X8, X0
	TESTQ  R8, R8
	JZ     rgbout
	MOVO   X0, X1
	PSLLO  $4, X1
	PADDB  X1, X0
	MOVO   X0, X1
	PSLLO  $8, X1
	PADDB  X1, X0
	PADDB  X15, X0
	PSHUFD $0xff, X0, X15

rgbout:
	POR   X13, X0
	MOVOU X0, (DI)
	ADDQ  DX, SI
	ADDQ  $16, DI
	DECQ  CX
	JNZ   rgbnext
	RET

// UNDO runs one group of undo8/undo16: X0 in, X0 out (blended),
// X15 carry. ADD is PADDB or PADDW.
#define UNDO(ADD) \
	MOVO   X0, X7 \
	MOVO   X0, X1 \
	PSHUFB X8, X1 \
	ADD    X1, X0 \
	MOVO   X0, X1 \
	PSHUFB X9, X1 \
	ADD    X1, X0 \
	MOVO   X0, X1 \
	PSHUFB X10, X1 \
	ADD    X1, X0 \
	MOVO   X0, X1 \
	PSHUFB X11, X1 \
	ADD    X1, X0 \
	ADD    X15, X0 \
	MOVO   X0, X15 \
	PSHUFB X12, X15 \
	PAND   X13, X0 \
	MOVO   X13, X2 \
	PANDN  X7, X2 \
	POR    X2, X0

// func undo8(p *byte, n, step int, t *[6][16]byte)
// The next group loads before this one stores (they overlap
// when g < 16), so it sees its own input bytes.
TEXT ·undo8(SB), NOSPLIT, $0-32
	MOVQ  p+0(FP), DI
	MOVQ  n+8(FP), CX
	MOVQ  step+16(FP), DX
	MOVQ  t+24(FP), AX
	MOVOU 0(AX), X8
	MOVOU 16(AX), X9
	MOVOU 32(AX), X10
	MOVOU 48(AX), X11
	MOVOU 64(AX), X12
	MOVOU 80(AX), X13
	PXOR  X15, X15
	MOVOU (DI), X0

u8next:
	UNDO(PADDB)
	DECQ  CX
	JZ    u8last
	MOVOU (DI)(DX*1), X6
	MOVOU X0, (DI)
	MOVO  X6, X0
	ADDQ  DX, DI
	JMP   u8next

u8last:
	MOVOU X0, (DI)
	RET

// func undo16(p *byte, n, step int, t *[6][16]byte)
TEXT ·undo16(SB), NOSPLIT, $0-32
	MOVQ  p+0(FP), DI
	MOVQ  n+8(FP), CX
	MOVQ  step+16(FP), DX
	MOVQ  t+24(FP), AX
	MOVOU 0(AX), X8
	MOVOU 16(AX), X9
	MOVOU 32(AX), X10
	MOVOU 48(AX), X11
	MOVOU 64(AX), X12
	MOVOU 80(AX), X13
	PXOR  X15, X15
	MOVOU (DI), X0

u16next:
	UNDO(PADDW)
	DECQ  CX
	JZ    u16last
	MOVOU (DI)(DX*1), X6
	MOVOU X0, (DI)
	MOVO  X6, X0
	ADDQ  DX, DI
	JMP   u16next

u16last:
	MOVOU X0, (DI)
	RET

// func cpuFeatures() (ssse3 bool)
TEXT ·cpuFeatures(SB), NOSPLIT, $0-1
	MOVL  $1, AX
	XORL  CX, CX
	CPUID
	SHRL  $9, CX
	ANDL  $1, CX
	MOVB  CX, ssse3+0(FP)
	RET
