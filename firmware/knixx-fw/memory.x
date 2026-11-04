/* knixx thunder with its own bootloader (firmware/knixx-boot): the
   bootloader in the first 64 KiB, the application's 256-byte header at
   0x10000, and the application from 0x10100 to the end of flash. The
   vector table at 0x10100 has the 256-byte alignment that 64 vectors need;
   cortex-m-rt's set-vtor points VTOR at it.

   The panic record goes in PANIC, at the bottom of RAM, after the double
   tap's word of the bootloader at 0x20000000; the bootloader keeps clear of
   both, so a record survives the reset through it. flip-link puts the stack
   at the bottom of RAM, from 0x20000108 up, so the stack never reaches the
   record. */
MEMORY
{
  FLASH : ORIGIN = 0x00010100, LENGTH = 0x00100000 - 0x00010100
  PANIC : ORIGIN = 0x20000008, LENGTH = 0x100
  RAM   : ORIGIN = 0x20000108, LENGTH = 0x20040000 - 0x20000108
}

SECTIONS
{
  .panic (NOLOAD) : ALIGN(4)
  {
    KEEP(*(.panic .panic.*));
  } > PANIC
} INSERT AFTER .uninit;
