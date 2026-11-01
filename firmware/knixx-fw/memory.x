/* Adafruit Feather nRF52840 with its stock flash layout: the MBR and the
   S140 6.1.1 SoftDevice below 0x26000 stay untouched (the SoftDevice is
   never enabled), user data from 0xED000, the UF2 bootloader from 0xF4000.
   RAM starts after the MBR's 8 bytes; the SoftDevice's RAM is not needed.

   The panic record goes in PANIC, at the bottom of RAM, which the UF2
   bootloader leaves alone outside DFU mode: its RAM is 0x20008000 and up,
   with its stack at the top of RAM, so a record there would not survive the
   reset through the bootloader. flip-link puts the stack at the bottom of
   RAM, from 0x20000108 up, so the stack never reaches the record. */
MEMORY
{
  FLASH : ORIGIN = 0x00026000, LENGTH = 0x000ED000 - 0x00026000
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
