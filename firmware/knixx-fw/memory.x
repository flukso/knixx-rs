/* Adafruit Feather nRF52840 with its stock flash layout: the MBR and the
   S140 6.1.1 SoftDevice below 0x26000 stay untouched (the SoftDevice is
   never enabled), user data from 0xED000, the UF2 bootloader from 0xF4000.
   RAM starts after the MBR's 8 bytes; the SoftDevice's RAM is not needed. */
MEMORY
{
  FLASH : ORIGIN = 0x00026000, LENGTH = 0x000ED000 - 0x00026000
  RAM   : ORIGIN = 0x20000008, LENGTH = 0x20040000 - 0x20000008
}
