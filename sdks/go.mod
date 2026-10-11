// Makes the Go SDK importable as a module — both in-repo (the examples use a
// `replace` to this directory) and externally (`go get github.com/bkhq/lode/sdks`).
// Stdlib only, no dependencies. Copying lode.go and the three lock_*.go files
// into your own module works just as well; this go.mod doesn't change that.
module github.com/bkhq/lode/sdks

go 1.22
