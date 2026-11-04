/* The bootloader: the first 64 KiB of flash, below the application's header
   at 0x10000. In RAM it keeps clear of the bottom 0x108 bytes: the double
   tap's word at 0x20000000 and the application's panic record at
   0x20000008, which survive the resets through the bootloader. flip-link
   puts its stack at the bottom, so an overflow faults before it reaches
   them. */
MEMORY
{
  FLASH : ORIGIN = 0x00000000, LENGTH = 0x00010000
  RAM   : ORIGIN = 0x20000108, LENGTH = 0x20040000 - 0x20000108
}
