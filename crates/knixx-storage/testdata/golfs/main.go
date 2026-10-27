// Command golfs makes and reads the littlefs images of knixx-storage's
// tests with the Go firmware's littlefs (tinygo.org/x/tinyfs v0.5.0) and
// its geometry, to show that the Go and Rust firmware share the
// configuration file.
//
//	go run . write ../go.img    littlefs with /knixx.ini holding the test config
//	go run . read ../rust.img   prints /knixx.ini from an image made in Rust
//
// An image is sparse: records of a little-endian uint32 sector number and
// the 4096 bytes of that sector, for the sectors that are not blank.
package main

import (
	"encoding/binary"
	"fmt"
	"io"
	"os"

	"tinygo.org/x/tinyfs/littlefs"
)

const (
	pageSize    = 256
	sectorSize  = 4096
	sectorCount = 512
	configPath  = "/knixx.ini"
	testConfig  = "[device]\nindividual = 7.8.9\n"
)

// flash is a NOR flash in RAM, as the GD25Q16C: writes can only clear bits.
type flash []byte

func (f flash) ReadAt(p []byte, off int64) (int, error) { return copy(p, f[off:]), nil }

func (f flash) WriteAt(p []byte, off int64) (int, error) {
	for i, b := range p {
		if b&^f[int(off)+i] != 0 {
			return 0, fmt.Errorf("write over unerased data at %#x", int(off)+i)
		}
		f[int(off)+i] &= b
	}
	return len(p), nil
}

func (f flash) Size() int64           { return int64(len(f)) }
func (f flash) WriteBlockSize() int64 { return pageSize }
func (f flash) EraseBlockSize() int64 { return sectorSize }

func (f flash) EraseBlocks(start, n int64) error {
	for i := start * sectorSize; i < (start+n)*sectorSize; i++ {
		f[i] = 0xFF
	}
	return nil
}

func blank() flash {
	f := make(flash, sectorSize*sectorCount)
	for i := range f {
		f[i] = 0xFF
	}
	return f
}

// fs returns the littlefs on f, configured as the Go firmware does.
func fs(f flash) *littlefs.LFS {
	return littlefs.New(f).Configure(&littlefs.Config{CacheSize: 512, LookaheadSize: 512, BlockCycles: 100})
}

func writeImage(path string) error {
	f := blank()
	lfs := fs(f)
	if err := lfs.Format(); err != nil {
		return err
	}
	if err := lfs.Mount(); err != nil {
		return err
	}
	file, err := lfs.OpenFile(configPath, os.O_CREATE|os.O_WRONLY|os.O_TRUNC)
	if err != nil {
		return err
	}
	if _, err := file.Write([]byte(testConfig)); err != nil {
		return err
	}
	if err := file.Close(); err != nil {
		return err
	}
	if err := lfs.Unmount(); err != nil {
		return err
	}
	var img []byte
	for n := 0; n < sectorCount; n++ {
		s := f[n*sectorSize : (n+1)*sectorSize]
		for _, b := range s {
			if b != 0xFF {
				img = binary.LittleEndian.AppendUint32(img, uint32(n))
				img = append(img, s...)
				break
			}
		}
	}
	return os.WriteFile(path, img, 0o644)
}

func readImage(path string) error {
	img, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	f := blank()
	for len(img) >= 4+sectorSize {
		n := binary.LittleEndian.Uint32(img)
		copy(f[n*sectorSize:], img[4:4+sectorSize])
		img = img[4+sectorSize:]
	}
	lfs := fs(f)
	if err := lfs.Mount(); err != nil {
		return fmt.Errorf("mount: %w", err)
	}
	file, err := lfs.OpenFile(configPath, os.O_RDWR)
	if err != nil {
		return err
	}
	data, err := io.ReadAll(file)
	if err != nil {
		return err
	}
	if string(data) != testConfig {
		return fmt.Errorf("%s holds %q, want %q", configPath, data, testConfig)
	}
	fmt.Printf("%s: %q\n", configPath, data)
	return nil
}

func main() {
	if len(os.Args) != 3 {
		fmt.Fprintln(os.Stderr, "usage: golfs write|read <image>")
		os.Exit(2)
	}
	var err error
	switch os.Args[1] {
	case "write":
		err = writeImage(os.Args[2])
	case "read":
		err = readImage(os.Args[2])
	default:
		err = fmt.Errorf("unknown command %q", os.Args[1])
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "golfs:", err)
		os.Exit(1)
	}
}
