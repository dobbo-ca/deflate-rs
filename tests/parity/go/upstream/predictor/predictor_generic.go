//go:build purego || !(amd64 || arm64)

package predictor

func expandVec(_, _ []byte, _, _ int, _, _ bool) int { return 0 }

func undoVec([]byte, int, int) int { return 0 }
