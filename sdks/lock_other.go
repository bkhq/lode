//go:build !unix && !windows

package lode

// lockState is a no-op on platforms lode does not run on: a process there is never supervised and
// never shares state.json with lode. The package compiles so cross-platform apps can vendor this
// SDK and branch on IsSupervised.
func lockState(string) func() { return func() {} }
