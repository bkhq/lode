//go:build windows

package lode

import (
	"os"
	"syscall"
	"unsafe"
)

// LockFileEx is not in package syscall; it is resolved from kernel32 so the SDK stays stdlib-only.
var procLockFileEx = syscall.NewLazyDLL("kernel32.dll").NewProc("LockFileEx")

const lockfileExclusiveLock = 0x2

// lockState takes lode's lock on the sibling lock file and returns the unlock function. lode locks
// the whole file with LockFileEx; so does this. Closing the handle releases the lock.
// Best effort: a lock that cannot be taken still runs the read-modify-write, as lode tolerates.
func lockState(path string) func() {
	lf, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE|os.O_APPEND, 0o644)
	if err != nil {
		return func() {}
	}
	var overlapped syscall.Overlapped
	ok, _, _ := procLockFileEx.Call(
		lf.Fd(),
		lockfileExclusiveLock,
		0,
		0xFFFFFFFF,
		0xFFFFFFFF,
		uintptr(unsafe.Pointer(&overlapped)),
	)
	if ok == 0 {
		_ = lf.Close()
		return func() {}
	}
	return func() { _ = lf.Close() }
}
