/* LM3S6965 memory layout for QEMU testing */
/* 256KB Flash, 64KB SRAM with 1KB reserved for PERSIST */

MEMORY
{
    FLASH   : ORIGIN = 0x00000000, LENGTH = 256K
    RAM     : ORIGIN = 0x20000000, LENGTH = 63K
    PERSIST : ORIGIN = 0x2000FC00, LENGTH = 1K
}

/* defmt-persist linker symbols */
__defmt_persist_start = ORIGIN(PERSIST);
__defmt_persist_end = ORIGIN(PERSIST) + LENGTH(PERSIST);
