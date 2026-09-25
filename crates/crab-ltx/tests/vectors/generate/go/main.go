// Writes the superfly/ltx reference vector with the same library Litestream
// uses (v0.5.2). Run from this directory:
//
//	go run . ../../   (relative output directory)
//
// See ../vectors/README.md for provenance and the regeneration command.
package main

import (
	"bytes"
	"crypto/sha256"
	"fmt"
	"os"
	"path/filepath"

	"github.com/superfly/ltx"
)

// page mirrors the byte pattern the Crab vector test expects.
func page(pageSize uint32, pgno uint32) []byte {
	data := make([]byte, pageSize)
	for index := range data {
		data[index] = byte((pgno*37 + uint32(index)) % 251)
	}
	return data
}

// deltaPage is the replacement body for page 2 in the delta vector.
func deltaPage(pageSize uint32, pgno uint32) []byte {
	data := page(pageSize, pgno)
	for index := range data {
		data[index] = 255 - data[index]
	}
	return data
}

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: go run . <output directory>")
		os.Exit(2)
	}
	const pageSize = 512
	const commit = 3
	writeSnapshot(os.Args[1], pageSize, commit)
	writeDelta(os.Args[1], pageSize, commit)
}

func writeSnapshot(output string, pageSize uint32, commit uint32) {
	path := filepath.Join(output, "superfly-ltx-v0.5.2-snapshot-block-512.ltx")
	file, err := os.Create(path)
	if err != nil {
		panic(err)
	}
	encoder, err := ltx.NewEncoder(file)
	if err != nil {
		panic(err)
	}

	var image bytes.Buffer
	for pgno := uint32(1); pgno <= commit; pgno++ {
		image.Write(page(pageSize, pgno))
	}
	// The reference implementation computes the rolling database checksum; a
	// caller hands it to the encoder instead of deriving it by hand.
	checksum, err := ltx.ChecksumReader(bytes.NewReader(image.Bytes()), int(pageSize))
	if err != nil {
		panic(err)
	}

	header := ltx.Header{
		Version:          3,
		Flags:            0,
		PageSize:         pageSize,
		Commit:           commit,
		MinTXID:          1,
		MaxTXID:          1,
		Timestamp:        1700000000000,
		PreApplyChecksum: 0,
	}
	if err := encoder.EncodeHeader(header); err != nil {
		panic(err)
	}
	for pgno := uint32(1); pgno <= commit; pgno++ {
		if err := encoder.EncodePage(ltx.PageHeader{Pgno: pgno}, page(pageSize, pgno)); err != nil {
			panic(err)
		}
	}
	encoder.SetPostApplyChecksum(checksum)
	if err := encoder.Close(); err != nil {
		panic(err)
	}
	if err := file.Close(); err != nil {
		panic(err)
	}
	bytes, err := os.ReadFile(path)
	if err != nil {
		panic(err)
	}
	fmt.Printf("%s: %d bytes checksum=%#x sha256=%x\n",
		path, len(bytes), uint64(checksum), sha256.Sum256(bytes))
}

// writeDelta emits one reference-encoded successor file: it replaces page 2 of
// the snapshot and publishes the resulting database checksum.
func writeDelta(output string, pageSize uint32, commit uint32) {
	old := page(pageSize, 2)
	next := deltaPage(pageSize, 2)
	image := append(append(page(pageSize, 1), old...), page(pageSize, 3)...)
	pre, err := ltx.ChecksumReader(bytes.NewReader(image), int(pageSize))
	if err != nil {
		panic(err)
	}
	// The successor checksum removes the replaced page and adds its replacement.
	post := ltx.Checksum(
		uint64(ltx.ChecksumFlag) |
			(uint64(pre) ^ uint64(ltx.ChecksumPage(2, old)) ^ uint64(ltx.ChecksumPage(2, next))),
	)

	path := filepath.Join(output, "superfly-ltx-v0.5.2-delta-2-2-512.ltx")
	file, err := os.Create(path)
	if err != nil {
		panic(err)
	}
	encoder, err := ltx.NewEncoder(file)
	if err != nil {
		panic(err)
	}
	header := ltx.Header{
		Version:          3,
		Flags:            0,
		PageSize:         pageSize,
		Commit:           commit,
		MinTXID:          2,
		MaxTXID:          2,
		Timestamp:        1700000000000,
		PreApplyChecksum: pre,
	}
	if err := encoder.EncodeHeader(header); err != nil {
		panic(err)
	}
	if err := encoder.EncodePage(ltx.PageHeader{Pgno: 2}, next); err != nil {
		panic(err)
	}
	encoder.SetPostApplyChecksum(post)
	if err := encoder.Close(); err != nil {
		panic(err)
	}
	if err := file.Close(); err != nil {
		panic(err)
	}
	bytes, err := os.ReadFile(path)
	if err != nil {
		panic(err)
	}
	fmt.Printf("%s: %d bytes pre=%#x post=%#x sha256=%x\n",
		path, len(bytes), uint64(pre), uint64(post), sha256.Sum256(bytes))
}
