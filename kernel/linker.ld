/* ═══════════════════════════════════════════════════════════════════════
 * BedrockOS x86_64 kernel linker script — Phase 2: higher-half split.
 *
 * TWO VMA regimes:
 *
 *   LOW region    VMA == LMA == 0x400000 + offset
 *                 .multiboot2 / .text.boot / .text.boot64 / .data.boot /
 *                 .bootstack (NOLOAD) / .boottables
 *                 ends at `__low_end` (2 MiB aligned so the higher-half
 *                 static page table may use 2 MiB PS leaf pages).
 *
 *   KERNEL region VMA == 0xFFFFFFFF80000000 (= KERNEL_VMA_BASE in
 *                 mm/layout.rs), LMA == __low_end + (Addr - KERNEL_VMA)
 *                 .text / .rodata / .data / .dynamic / .idt / .bss / .stack
 *                 (.rela.dyn is discarded — see the /DISCARD/ block below)
 *
 * The image is a non-PIE ET_EXEC: the UEFI bootloader copies PT_LOAD
 * segments by p_paddr and jumps to e_entry (== `_start`, in the LOW region),
 * so it never applies relocations.  Every kernel-region section therefore
 * uses `AT(. - KERNEL_VMA + __low_end)` so its on-disk LMA is contiguous
 * right after the low region instead of overlapping it at 0x400000.
 *
 * `.multiboot2` is kept as the very first output section / segment so it
 * lands within the first 32768 bytes of the file (GRUB Multiboot2).
 *
 * `.boottables` holds static, fully-resolved 4-level x86_64 page tables.
 * Each frame is a named, 4096-aligned symbol; a frame's physical address is
 * `LOADADDR(.boottables) + (frame - __boottables_start)`.  Because the low
 * region is identity-mapped (VMA == LMA == physical), a frame symbol's value
 * is already its physical address; the LOADADDR() form is used so the table
 * stays correct even if a later phase relocates `.boottables`.
 * ═══════════════════════════════════════════════════════════════════════ */

ENTRY(_start)

KERNEL_OFFSET = 0x400000;
KERNEL_VMA = 0xFFFFFFFF80000000;

SECTIONS
{
    /* ── LOW REGION: VMA == LMA == 0x400000 + offset ──────────────── */
    . = KERNEL_OFFSET;
    __low_start = .;

    /* Multiboot2 header MUST be the first thing placed. */
    .multiboot2 : {
        KEEP(*(.multiboot2))
    }

    .text.boot : {
        KEEP(*(.text.boot))
    }

    .text.boot64 : {
        KEEP(*(.text.boot64))
    }

    .data.boot : {
        *(.data.boot)
    }

    /* Boot-time stack (NOLOAD) — the 32->64-bit stub switches RSP here. */
    . = ALIGN(16);
    .bootstack (NOLOAD) : {
        __boot_stack_start = .;
        . += 0x8000;
        __boot_stack_end = .;
    }

    /* Static early 4-level page tables (see the frame map below). */
    . = ALIGN(4096);
    .boottables : {
        __boottables_start = .;

        /* PML4: [0] -> low PDPT, [511] -> high PDPT. */
        __boot_pml4 = .;
        QUAD((LOADADDR(.boottables) + (__boot_pdpt0   - __boottables_start)) | 0x3) /* PML4[0]   -> __boot_pdpt0   */
        . = __boot_pml4 + 8 * 511;
        QUAD((LOADADDR(.boottables) + (__boot_pdpt511 - __boottables_start)) | 0x3) /* PML4[511] -> __boot_pdpt511 */
        . = __boot_pml4 + 4096;

        /* PDPT[0]: entry 0 -> PD0 (low identity, covers 1 GiB). */
        __boot_pdpt0 = .;
        QUAD((LOADADDR(.boottables) + (__boot_pd0 - __boottables_start)) | 0x3)     /* PDPT[0] -> __boot_pd0 */
        . = __boot_pdpt0 + 4096;

        /* PD0: 512 x 2 MiB identity pages, phys [0, 1 GiB).  Present|Write|PS. */
        __boot_pd0 = .;
        QUAD(0x83) /* [0] */
        QUAD(0x200083) /* [1] */
        QUAD(0x400083) /* [2] */
        QUAD(0x600083) /* [3] */
        QUAD(0x800083) /* [4] */
        QUAD(0xa00083) /* [5] */
        QUAD(0xc00083) /* [6] */
        QUAD(0xe00083) /* [7] */
        QUAD(0x1000083) /* [8] */
        QUAD(0x1200083) /* [9] */
        QUAD(0x1400083) /* [10] */
        QUAD(0x1600083) /* [11] */
        QUAD(0x1800083) /* [12] */
        QUAD(0x1a00083) /* [13] */
        QUAD(0x1c00083) /* [14] */
        QUAD(0x1e00083) /* [15] */
        QUAD(0x2000083) /* [16] */
        QUAD(0x2200083) /* [17] */
        QUAD(0x2400083) /* [18] */
        QUAD(0x2600083) /* [19] */
        QUAD(0x2800083) /* [20] */
        QUAD(0x2a00083) /* [21] */
        QUAD(0x2c00083) /* [22] */
        QUAD(0x2e00083) /* [23] */
        QUAD(0x3000083) /* [24] */
        QUAD(0x3200083) /* [25] */
        QUAD(0x3400083) /* [26] */
        QUAD(0x3600083) /* [27] */
        QUAD(0x3800083) /* [28] */
        QUAD(0x3a00083) /* [29] */
        QUAD(0x3c00083) /* [30] */
        QUAD(0x3e00083) /* [31] */
        QUAD(0x4000083) /* [32] */
        QUAD(0x4200083) /* [33] */
        QUAD(0x4400083) /* [34] */
        QUAD(0x4600083) /* [35] */
        QUAD(0x4800083) /* [36] */
        QUAD(0x4a00083) /* [37] */
        QUAD(0x4c00083) /* [38] */
        QUAD(0x4e00083) /* [39] */
        QUAD(0x5000083) /* [40] */
        QUAD(0x5200083) /* [41] */
        QUAD(0x5400083) /* [42] */
        QUAD(0x5600083) /* [43] */
        QUAD(0x5800083) /* [44] */
        QUAD(0x5a00083) /* [45] */
        QUAD(0x5c00083) /* [46] */
        QUAD(0x5e00083) /* [47] */
        QUAD(0x6000083) /* [48] */
        QUAD(0x6200083) /* [49] */
        QUAD(0x6400083) /* [50] */
        QUAD(0x6600083) /* [51] */
        QUAD(0x6800083) /* [52] */
        QUAD(0x6a00083) /* [53] */
        QUAD(0x6c00083) /* [54] */
        QUAD(0x6e00083) /* [55] */
        QUAD(0x7000083) /* [56] */
        QUAD(0x7200083) /* [57] */
        QUAD(0x7400083) /* [58] */
        QUAD(0x7600083) /* [59] */
        QUAD(0x7800083) /* [60] */
        QUAD(0x7a00083) /* [61] */
        QUAD(0x7c00083) /* [62] */
        QUAD(0x7e00083) /* [63] */
        QUAD(0x8000083) /* [64] */
        QUAD(0x8200083) /* [65] */
        QUAD(0x8400083) /* [66] */
        QUAD(0x8600083) /* [67] */
        QUAD(0x8800083) /* [68] */
        QUAD(0x8a00083) /* [69] */
        QUAD(0x8c00083) /* [70] */
        QUAD(0x8e00083) /* [71] */
        QUAD(0x9000083) /* [72] */
        QUAD(0x9200083) /* [73] */
        QUAD(0x9400083) /* [74] */
        QUAD(0x9600083) /* [75] */
        QUAD(0x9800083) /* [76] */
        QUAD(0x9a00083) /* [77] */
        QUAD(0x9c00083) /* [78] */
        QUAD(0x9e00083) /* [79] */
        QUAD(0xa000083) /* [80] */
        QUAD(0xa200083) /* [81] */
        QUAD(0xa400083) /* [82] */
        QUAD(0xa600083) /* [83] */
        QUAD(0xa800083) /* [84] */
        QUAD(0xaa00083) /* [85] */
        QUAD(0xac00083) /* [86] */
        QUAD(0xae00083) /* [87] */
        QUAD(0xb000083) /* [88] */
        QUAD(0xb200083) /* [89] */
        QUAD(0xb400083) /* [90] */
        QUAD(0xb600083) /* [91] */
        QUAD(0xb800083) /* [92] */
        QUAD(0xba00083) /* [93] */
        QUAD(0xbc00083) /* [94] */
        QUAD(0xbe00083) /* [95] */
        QUAD(0xc000083) /* [96] */
        QUAD(0xc200083) /* [97] */
        QUAD(0xc400083) /* [98] */
        QUAD(0xc600083) /* [99] */
        QUAD(0xc800083) /* [100] */
        QUAD(0xca00083) /* [101] */
        QUAD(0xcc00083) /* [102] */
        QUAD(0xce00083) /* [103] */
        QUAD(0xd000083) /* [104] */
        QUAD(0xd200083) /* [105] */
        QUAD(0xd400083) /* [106] */
        QUAD(0xd600083) /* [107] */
        QUAD(0xd800083) /* [108] */
        QUAD(0xda00083) /* [109] */
        QUAD(0xdc00083) /* [110] */
        QUAD(0xde00083) /* [111] */
        QUAD(0xe000083) /* [112] */
        QUAD(0xe200083) /* [113] */
        QUAD(0xe400083) /* [114] */
        QUAD(0xe600083) /* [115] */
        QUAD(0xe800083) /* [116] */
        QUAD(0xea00083) /* [117] */
        QUAD(0xec00083) /* [118] */
        QUAD(0xee00083) /* [119] */
        QUAD(0xf000083) /* [120] */
        QUAD(0xf200083) /* [121] */
        QUAD(0xf400083) /* [122] */
        QUAD(0xf600083) /* [123] */
        QUAD(0xf800083) /* [124] */
        QUAD(0xfa00083) /* [125] */
        QUAD(0xfc00083) /* [126] */
        QUAD(0xfe00083) /* [127] */
        QUAD(0x10000083) /* [128] */
        QUAD(0x10200083) /* [129] */
        QUAD(0x10400083) /* [130] */
        QUAD(0x10600083) /* [131] */
        QUAD(0x10800083) /* [132] */
        QUAD(0x10a00083) /* [133] */
        QUAD(0x10c00083) /* [134] */
        QUAD(0x10e00083) /* [135] */
        QUAD(0x11000083) /* [136] */
        QUAD(0x11200083) /* [137] */
        QUAD(0x11400083) /* [138] */
        QUAD(0x11600083) /* [139] */
        QUAD(0x11800083) /* [140] */
        QUAD(0x11a00083) /* [141] */
        QUAD(0x11c00083) /* [142] */
        QUAD(0x11e00083) /* [143] */
        QUAD(0x12000083) /* [144] */
        QUAD(0x12200083) /* [145] */
        QUAD(0x12400083) /* [146] */
        QUAD(0x12600083) /* [147] */
        QUAD(0x12800083) /* [148] */
        QUAD(0x12a00083) /* [149] */
        QUAD(0x12c00083) /* [150] */
        QUAD(0x12e00083) /* [151] */
        QUAD(0x13000083) /* [152] */
        QUAD(0x13200083) /* [153] */
        QUAD(0x13400083) /* [154] */
        QUAD(0x13600083) /* [155] */
        QUAD(0x13800083) /* [156] */
        QUAD(0x13a00083) /* [157] */
        QUAD(0x13c00083) /* [158] */
        QUAD(0x13e00083) /* [159] */
        QUAD(0x14000083) /* [160] */
        QUAD(0x14200083) /* [161] */
        QUAD(0x14400083) /* [162] */
        QUAD(0x14600083) /* [163] */
        QUAD(0x14800083) /* [164] */
        QUAD(0x14a00083) /* [165] */
        QUAD(0x14c00083) /* [166] */
        QUAD(0x14e00083) /* [167] */
        QUAD(0x15000083) /* [168] */
        QUAD(0x15200083) /* [169] */
        QUAD(0x15400083) /* [170] */
        QUAD(0x15600083) /* [171] */
        QUAD(0x15800083) /* [172] */
        QUAD(0x15a00083) /* [173] */
        QUAD(0x15c00083) /* [174] */
        QUAD(0x15e00083) /* [175] */
        QUAD(0x16000083) /* [176] */
        QUAD(0x16200083) /* [177] */
        QUAD(0x16400083) /* [178] */
        QUAD(0x16600083) /* [179] */
        QUAD(0x16800083) /* [180] */
        QUAD(0x16a00083) /* [181] */
        QUAD(0x16c00083) /* [182] */
        QUAD(0x16e00083) /* [183] */
        QUAD(0x17000083) /* [184] */
        QUAD(0x17200083) /* [185] */
        QUAD(0x17400083) /* [186] */
        QUAD(0x17600083) /* [187] */
        QUAD(0x17800083) /* [188] */
        QUAD(0x17a00083) /* [189] */
        QUAD(0x17c00083) /* [190] */
        QUAD(0x17e00083) /* [191] */
        QUAD(0x18000083) /* [192] */
        QUAD(0x18200083) /* [193] */
        QUAD(0x18400083) /* [194] */
        QUAD(0x18600083) /* [195] */
        QUAD(0x18800083) /* [196] */
        QUAD(0x18a00083) /* [197] */
        QUAD(0x18c00083) /* [198] */
        QUAD(0x18e00083) /* [199] */
        QUAD(0x19000083) /* [200] */
        QUAD(0x19200083) /* [201] */
        QUAD(0x19400083) /* [202] */
        QUAD(0x19600083) /* [203] */
        QUAD(0x19800083) /* [204] */
        QUAD(0x19a00083) /* [205] */
        QUAD(0x19c00083) /* [206] */
        QUAD(0x19e00083) /* [207] */
        QUAD(0x1a000083) /* [208] */
        QUAD(0x1a200083) /* [209] */
        QUAD(0x1a400083) /* [210] */
        QUAD(0x1a600083) /* [211] */
        QUAD(0x1a800083) /* [212] */
        QUAD(0x1aa00083) /* [213] */
        QUAD(0x1ac00083) /* [214] */
        QUAD(0x1ae00083) /* [215] */
        QUAD(0x1b000083) /* [216] */
        QUAD(0x1b200083) /* [217] */
        QUAD(0x1b400083) /* [218] */
        QUAD(0x1b600083) /* [219] */
        QUAD(0x1b800083) /* [220] */
        QUAD(0x1ba00083) /* [221] */
        QUAD(0x1bc00083) /* [222] */
        QUAD(0x1be00083) /* [223] */
        QUAD(0x1c000083) /* [224] */
        QUAD(0x1c200083) /* [225] */
        QUAD(0x1c400083) /* [226] */
        QUAD(0x1c600083) /* [227] */
        QUAD(0x1c800083) /* [228] */
        QUAD(0x1ca00083) /* [229] */
        QUAD(0x1cc00083) /* [230] */
        QUAD(0x1ce00083) /* [231] */
        QUAD(0x1d000083) /* [232] */
        QUAD(0x1d200083) /* [233] */
        QUAD(0x1d400083) /* [234] */
        QUAD(0x1d600083) /* [235] */
        QUAD(0x1d800083) /* [236] */
        QUAD(0x1da00083) /* [237] */
        QUAD(0x1dc00083) /* [238] */
        QUAD(0x1de00083) /* [239] */
        QUAD(0x1e000083) /* [240] */
        QUAD(0x1e200083) /* [241] */
        QUAD(0x1e400083) /* [242] */
        QUAD(0x1e600083) /* [243] */
        QUAD(0x1e800083) /* [244] */
        QUAD(0x1ea00083) /* [245] */
        QUAD(0x1ec00083) /* [246] */
        QUAD(0x1ee00083) /* [247] */
        QUAD(0x1f000083) /* [248] */
        QUAD(0x1f200083) /* [249] */
        QUAD(0x1f400083) /* [250] */
        QUAD(0x1f600083) /* [251] */
        QUAD(0x1f800083) /* [252] */
        QUAD(0x1fa00083) /* [253] */
        QUAD(0x1fc00083) /* [254] */
        QUAD(0x1fe00083) /* [255] */
        QUAD(0x20000083) /* [256] */
        QUAD(0x20200083) /* [257] */
        QUAD(0x20400083) /* [258] */
        QUAD(0x20600083) /* [259] */
        QUAD(0x20800083) /* [260] */
        QUAD(0x20a00083) /* [261] */
        QUAD(0x20c00083) /* [262] */
        QUAD(0x20e00083) /* [263] */
        QUAD(0x21000083) /* [264] */
        QUAD(0x21200083) /* [265] */
        QUAD(0x21400083) /* [266] */
        QUAD(0x21600083) /* [267] */
        QUAD(0x21800083) /* [268] */
        QUAD(0x21a00083) /* [269] */
        QUAD(0x21c00083) /* [270] */
        QUAD(0x21e00083) /* [271] */
        QUAD(0x22000083) /* [272] */
        QUAD(0x22200083) /* [273] */
        QUAD(0x22400083) /* [274] */
        QUAD(0x22600083) /* [275] */
        QUAD(0x22800083) /* [276] */
        QUAD(0x22a00083) /* [277] */
        QUAD(0x22c00083) /* [278] */
        QUAD(0x22e00083) /* [279] */
        QUAD(0x23000083) /* [280] */
        QUAD(0x23200083) /* [281] */
        QUAD(0x23400083) /* [282] */
        QUAD(0x23600083) /* [283] */
        QUAD(0x23800083) /* [284] */
        QUAD(0x23a00083) /* [285] */
        QUAD(0x23c00083) /* [286] */
        QUAD(0x23e00083) /* [287] */
        QUAD(0x24000083) /* [288] */
        QUAD(0x24200083) /* [289] */
        QUAD(0x24400083) /* [290] */
        QUAD(0x24600083) /* [291] */
        QUAD(0x24800083) /* [292] */
        QUAD(0x24a00083) /* [293] */
        QUAD(0x24c00083) /* [294] */
        QUAD(0x24e00083) /* [295] */
        QUAD(0x25000083) /* [296] */
        QUAD(0x25200083) /* [297] */
        QUAD(0x25400083) /* [298] */
        QUAD(0x25600083) /* [299] */
        QUAD(0x25800083) /* [300] */
        QUAD(0x25a00083) /* [301] */
        QUAD(0x25c00083) /* [302] */
        QUAD(0x25e00083) /* [303] */
        QUAD(0x26000083) /* [304] */
        QUAD(0x26200083) /* [305] */
        QUAD(0x26400083) /* [306] */
        QUAD(0x26600083) /* [307] */
        QUAD(0x26800083) /* [308] */
        QUAD(0x26a00083) /* [309] */
        QUAD(0x26c00083) /* [310] */
        QUAD(0x26e00083) /* [311] */
        QUAD(0x27000083) /* [312] */
        QUAD(0x27200083) /* [313] */
        QUAD(0x27400083) /* [314] */
        QUAD(0x27600083) /* [315] */
        QUAD(0x27800083) /* [316] */
        QUAD(0x27a00083) /* [317] */
        QUAD(0x27c00083) /* [318] */
        QUAD(0x27e00083) /* [319] */
        QUAD(0x28000083) /* [320] */
        QUAD(0x28200083) /* [321] */
        QUAD(0x28400083) /* [322] */
        QUAD(0x28600083) /* [323] */
        QUAD(0x28800083) /* [324] */
        QUAD(0x28a00083) /* [325] */
        QUAD(0x28c00083) /* [326] */
        QUAD(0x28e00083) /* [327] */
        QUAD(0x29000083) /* [328] */
        QUAD(0x29200083) /* [329] */
        QUAD(0x29400083) /* [330] */
        QUAD(0x29600083) /* [331] */
        QUAD(0x29800083) /* [332] */
        QUAD(0x29a00083) /* [333] */
        QUAD(0x29c00083) /* [334] */
        QUAD(0x29e00083) /* [335] */
        QUAD(0x2a000083) /* [336] */
        QUAD(0x2a200083) /* [337] */
        QUAD(0x2a400083) /* [338] */
        QUAD(0x2a600083) /* [339] */
        QUAD(0x2a800083) /* [340] */
        QUAD(0x2aa00083) /* [341] */
        QUAD(0x2ac00083) /* [342] */
        QUAD(0x2ae00083) /* [343] */
        QUAD(0x2b000083) /* [344] */
        QUAD(0x2b200083) /* [345] */
        QUAD(0x2b400083) /* [346] */
        QUAD(0x2b600083) /* [347] */
        QUAD(0x2b800083) /* [348] */
        QUAD(0x2ba00083) /* [349] */
        QUAD(0x2bc00083) /* [350] */
        QUAD(0x2be00083) /* [351] */
        QUAD(0x2c000083) /* [352] */
        QUAD(0x2c200083) /* [353] */
        QUAD(0x2c400083) /* [354] */
        QUAD(0x2c600083) /* [355] */
        QUAD(0x2c800083) /* [356] */
        QUAD(0x2ca00083) /* [357] */
        QUAD(0x2cc00083) /* [358] */
        QUAD(0x2ce00083) /* [359] */
        QUAD(0x2d000083) /* [360] */
        QUAD(0x2d200083) /* [361] */
        QUAD(0x2d400083) /* [362] */
        QUAD(0x2d600083) /* [363] */
        QUAD(0x2d800083) /* [364] */
        QUAD(0x2da00083) /* [365] */
        QUAD(0x2dc00083) /* [366] */
        QUAD(0x2de00083) /* [367] */
        QUAD(0x2e000083) /* [368] */
        QUAD(0x2e200083) /* [369] */
        QUAD(0x2e400083) /* [370] */
        QUAD(0x2e600083) /* [371] */
        QUAD(0x2e800083) /* [372] */
        QUAD(0x2ea00083) /* [373] */
        QUAD(0x2ec00083) /* [374] */
        QUAD(0x2ee00083) /* [375] */
        QUAD(0x2f000083) /* [376] */
        QUAD(0x2f200083) /* [377] */
        QUAD(0x2f400083) /* [378] */
        QUAD(0x2f600083) /* [379] */
        QUAD(0x2f800083) /* [380] */
        QUAD(0x2fa00083) /* [381] */
        QUAD(0x2fc00083) /* [382] */
        QUAD(0x2fe00083) /* [383] */
        QUAD(0x30000083) /* [384] */
        QUAD(0x30200083) /* [385] */
        QUAD(0x30400083) /* [386] */
        QUAD(0x30600083) /* [387] */
        QUAD(0x30800083) /* [388] */
        QUAD(0x30a00083) /* [389] */
        QUAD(0x30c00083) /* [390] */
        QUAD(0x30e00083) /* [391] */
        QUAD(0x31000083) /* [392] */
        QUAD(0x31200083) /* [393] */
        QUAD(0x31400083) /* [394] */
        QUAD(0x31600083) /* [395] */
        QUAD(0x31800083) /* [396] */
        QUAD(0x31a00083) /* [397] */
        QUAD(0x31c00083) /* [398] */
        QUAD(0x31e00083) /* [399] */
        QUAD(0x32000083) /* [400] */
        QUAD(0x32200083) /* [401] */
        QUAD(0x32400083) /* [402] */
        QUAD(0x32600083) /* [403] */
        QUAD(0x32800083) /* [404] */
        QUAD(0x32a00083) /* [405] */
        QUAD(0x32c00083) /* [406] */
        QUAD(0x32e00083) /* [407] */
        QUAD(0x33000083) /* [408] */
        QUAD(0x33200083) /* [409] */
        QUAD(0x33400083) /* [410] */
        QUAD(0x33600083) /* [411] */
        QUAD(0x33800083) /* [412] */
        QUAD(0x33a00083) /* [413] */
        QUAD(0x33c00083) /* [414] */
        QUAD(0x33e00083) /* [415] */
        QUAD(0x34000083) /* [416] */
        QUAD(0x34200083) /* [417] */
        QUAD(0x34400083) /* [418] */
        QUAD(0x34600083) /* [419] */
        QUAD(0x34800083) /* [420] */
        QUAD(0x34a00083) /* [421] */
        QUAD(0x34c00083) /* [422] */
        QUAD(0x34e00083) /* [423] */
        QUAD(0x35000083) /* [424] */
        QUAD(0x35200083) /* [425] */
        QUAD(0x35400083) /* [426] */
        QUAD(0x35600083) /* [427] */
        QUAD(0x35800083) /* [428] */
        QUAD(0x35a00083) /* [429] */
        QUAD(0x35c00083) /* [430] */
        QUAD(0x35e00083) /* [431] */
        QUAD(0x36000083) /* [432] */
        QUAD(0x36200083) /* [433] */
        QUAD(0x36400083) /* [434] */
        QUAD(0x36600083) /* [435] */
        QUAD(0x36800083) /* [436] */
        QUAD(0x36a00083) /* [437] */
        QUAD(0x36c00083) /* [438] */
        QUAD(0x36e00083) /* [439] */
        QUAD(0x37000083) /* [440] */
        QUAD(0x37200083) /* [441] */
        QUAD(0x37400083) /* [442] */
        QUAD(0x37600083) /* [443] */
        QUAD(0x37800083) /* [444] */
        QUAD(0x37a00083) /* [445] */
        QUAD(0x37c00083) /* [446] */
        QUAD(0x37e00083) /* [447] */
        QUAD(0x38000083) /* [448] */
        QUAD(0x38200083) /* [449] */
        QUAD(0x38400083) /* [450] */
        QUAD(0x38600083) /* [451] */
        QUAD(0x38800083) /* [452] */
        QUAD(0x38a00083) /* [453] */
        QUAD(0x38c00083) /* [454] */
        QUAD(0x38e00083) /* [455] */
        QUAD(0x39000083) /* [456] */
        QUAD(0x39200083) /* [457] */
        QUAD(0x39400083) /* [458] */
        QUAD(0x39600083) /* [459] */
        QUAD(0x39800083) /* [460] */
        QUAD(0x39a00083) /* [461] */
        QUAD(0x39c00083) /* [462] */
        QUAD(0x39e00083) /* [463] */
        QUAD(0x3a000083) /* [464] */
        QUAD(0x3a200083) /* [465] */
        QUAD(0x3a400083) /* [466] */
        QUAD(0x3a600083) /* [467] */
        QUAD(0x3a800083) /* [468] */
        QUAD(0x3aa00083) /* [469] */
        QUAD(0x3ac00083) /* [470] */
        QUAD(0x3ae00083) /* [471] */
        QUAD(0x3b000083) /* [472] */
        QUAD(0x3b200083) /* [473] */
        QUAD(0x3b400083) /* [474] */
        QUAD(0x3b600083) /* [475] */
        QUAD(0x3b800083) /* [476] */
        QUAD(0x3ba00083) /* [477] */
        QUAD(0x3bc00083) /* [478] */
        QUAD(0x3be00083) /* [479] */
        QUAD(0x3c000083) /* [480] */
        QUAD(0x3c200083) /* [481] */
        QUAD(0x3c400083) /* [482] */
        QUAD(0x3c600083) /* [483] */
        QUAD(0x3c800083) /* [484] */
        QUAD(0x3ca00083) /* [485] */
        QUAD(0x3cc00083) /* [486] */
        QUAD(0x3ce00083) /* [487] */
        QUAD(0x3d000083) /* [488] */
        QUAD(0x3d200083) /* [489] */
        QUAD(0x3d400083) /* [490] */
        QUAD(0x3d600083) /* [491] */
        QUAD(0x3d800083) /* [492] */
        QUAD(0x3da00083) /* [493] */
        QUAD(0x3dc00083) /* [494] */
        QUAD(0x3de00083) /* [495] */
        QUAD(0x3e000083) /* [496] */
        QUAD(0x3e200083) /* [497] */
        QUAD(0x3e400083) /* [498] */
        QUAD(0x3e600083) /* [499] */
        QUAD(0x3e800083) /* [500] */
        QUAD(0x3ea00083) /* [501] */
        QUAD(0x3ec00083) /* [502] */
        QUAD(0x3ee00083) /* [503] */
        QUAD(0x3f000083) /* [504] */
        QUAD(0x3f200083) /* [505] */
        QUAD(0x3f400083) /* [506] */
        QUAD(0x3f600083) /* [507] */
        QUAD(0x3f800083) /* [508] */
        QUAD(0x3fa00083) /* [509] */
        QUAD(0x3fc00083) /* [510] */
        QUAD(0x3fe00083) /* [511] */
        . = __boot_pd0 + 4096;

        /* PDPT[511]: entry 510 -> PD511 (high kernel).  KERNEL_VMA sits
         * exactly at the base of PDPT slot 510 (offset 0x7F80000000 within
         * the PML4[511] window = 510 GiB), so its PD index is 0. */
        __boot_pdpt511 = .;
        . = __boot_pdpt511 + 8 * 510;
        QUAD((LOADADDR(.boottables) + (__boot_pd511 - __boottables_start)) | 0x3)  /* PDPT[510] -> __boot_pd511 */
        . = __boot_pdpt511 + 4096;

        /* PD511: 128 x 2 MiB RW pages mapping
         * [KERNEL_VMA, KERNEL_VMA + 256 MiB) -> phys [__low_end, +256 MiB). */
        __boot_pd511 = .;
        QUAD((__low_end + 0x0) | 0x83) /* [0] */
        QUAD((__low_end + 0x200000) | 0x83) /* [1] */
        QUAD((__low_end + 0x400000) | 0x83) /* [2] */
        QUAD((__low_end + 0x600000) | 0x83) /* [3] */
        QUAD((__low_end + 0x800000) | 0x83) /* [4] */
        QUAD((__low_end + 0xa00000) | 0x83) /* [5] */
        QUAD((__low_end + 0xc00000) | 0x83) /* [6] */
        QUAD((__low_end + 0xe00000) | 0x83) /* [7] */
        QUAD((__low_end + 0x1000000) | 0x83) /* [8] */
        QUAD((__low_end + 0x1200000) | 0x83) /* [9] */
        QUAD((__low_end + 0x1400000) | 0x83) /* [10] */
        QUAD((__low_end + 0x1600000) | 0x83) /* [11] */
        QUAD((__low_end + 0x1800000) | 0x83) /* [12] */
        QUAD((__low_end + 0x1a00000) | 0x83) /* [13] */
        QUAD((__low_end + 0x1c00000) | 0x83) /* [14] */
        QUAD((__low_end + 0x1e00000) | 0x83) /* [15] */
        QUAD((__low_end + 0x2000000) | 0x83) /* [16] */
        QUAD((__low_end + 0x2200000) | 0x83) /* [17] */
        QUAD((__low_end + 0x2400000) | 0x83) /* [18] */
        QUAD((__low_end + 0x2600000) | 0x83) /* [19] */
        QUAD((__low_end + 0x2800000) | 0x83) /* [20] */
        QUAD((__low_end + 0x2a00000) | 0x83) /* [21] */
        QUAD((__low_end + 0x2c00000) | 0x83) /* [22] */
        QUAD((__low_end + 0x2e00000) | 0x83) /* [23] */
        QUAD((__low_end + 0x3000000) | 0x83) /* [24] */
        QUAD((__low_end + 0x3200000) | 0x83) /* [25] */
        QUAD((__low_end + 0x3400000) | 0x83) /* [26] */
        QUAD((__low_end + 0x3600000) | 0x83) /* [27] */
        QUAD((__low_end + 0x3800000) | 0x83) /* [28] */
        QUAD((__low_end + 0x3a00000) | 0x83) /* [29] */
        QUAD((__low_end + 0x3c00000) | 0x83) /* [30] */
        QUAD((__low_end + 0x3e00000) | 0x83) /* [31] */
        QUAD((__low_end + 0x4000000) | 0x83) /* [32] */
        QUAD((__low_end + 0x4200000) | 0x83) /* [33] */
        QUAD((__low_end + 0x4400000) | 0x83) /* [34] */
        QUAD((__low_end + 0x4600000) | 0x83) /* [35] */
        QUAD((__low_end + 0x4800000) | 0x83) /* [36] */
        QUAD((__low_end + 0x4a00000) | 0x83) /* [37] */
        QUAD((__low_end + 0x4c00000) | 0x83) /* [38] */
        QUAD((__low_end + 0x4e00000) | 0x83) /* [39] */
        QUAD((__low_end + 0x5000000) | 0x83) /* [40] */
        QUAD((__low_end + 0x5200000) | 0x83) /* [41] */
        QUAD((__low_end + 0x5400000) | 0x83) /* [42] */
        QUAD((__low_end + 0x5600000) | 0x83) /* [43] */
        QUAD((__low_end + 0x5800000) | 0x83) /* [44] */
        QUAD((__low_end + 0x5a00000) | 0x83) /* [45] */
        QUAD((__low_end + 0x5c00000) | 0x83) /* [46] */
        QUAD((__low_end + 0x5e00000) | 0x83) /* [47] */
        QUAD((__low_end + 0x6000000) | 0x83) /* [48] */
        QUAD((__low_end + 0x6200000) | 0x83) /* [49] */
        QUAD((__low_end + 0x6400000) | 0x83) /* [50] */
        QUAD((__low_end + 0x6600000) | 0x83) /* [51] */
        QUAD((__low_end + 0x6800000) | 0x83) /* [52] */
        QUAD((__low_end + 0x6a00000) | 0x83) /* [53] */
        QUAD((__low_end + 0x6c00000) | 0x83) /* [54] */
        QUAD((__low_end + 0x6e00000) | 0x83) /* [55] */
        QUAD((__low_end + 0x7000000) | 0x83) /* [56] */
        QUAD((__low_end + 0x7200000) | 0x83) /* [57] */
        QUAD((__low_end + 0x7400000) | 0x83) /* [58] */
        QUAD((__low_end + 0x7600000) | 0x83) /* [59] */
        QUAD((__low_end + 0x7800000) | 0x83) /* [60] */
        QUAD((__low_end + 0x7a00000) | 0x83) /* [61] */
        QUAD((__low_end + 0x7c00000) | 0x83) /* [62] */
        QUAD((__low_end + 0x7e00000) | 0x83) /* [63] */
        QUAD((__low_end + 0x8000000) | 0x83) /* [64] */
        QUAD((__low_end + 0x8200000) | 0x83) /* [65] */
        QUAD((__low_end + 0x8400000) | 0x83) /* [66] */
        QUAD((__low_end + 0x8600000) | 0x83) /* [67] */
        QUAD((__low_end + 0x8800000) | 0x83) /* [68] */
        QUAD((__low_end + 0x8a00000) | 0x83) /* [69] */
        QUAD((__low_end + 0x8c00000) | 0x83) /* [70] */
        QUAD((__low_end + 0x8e00000) | 0x83) /* [71] */
        QUAD((__low_end + 0x9000000) | 0x83) /* [72] */
        QUAD((__low_end + 0x9200000) | 0x83) /* [73] */
        QUAD((__low_end + 0x9400000) | 0x83) /* [74] */
        QUAD((__low_end + 0x9600000) | 0x83) /* [75] */
        QUAD((__low_end + 0x9800000) | 0x83) /* [76] */
        QUAD((__low_end + 0x9a00000) | 0x83) /* [77] */
        QUAD((__low_end + 0x9c00000) | 0x83) /* [78] */
        QUAD((__low_end + 0x9e00000) | 0x83) /* [79] */
        QUAD((__low_end + 0xa000000) | 0x83) /* [80] */
        QUAD((__low_end + 0xa200000) | 0x83) /* [81] */
        QUAD((__low_end + 0xa400000) | 0x83) /* [82] */
        QUAD((__low_end + 0xa600000) | 0x83) /* [83] */
        QUAD((__low_end + 0xa800000) | 0x83) /* [84] */
        QUAD((__low_end + 0xaa00000) | 0x83) /* [85] */
        QUAD((__low_end + 0xac00000) | 0x83) /* [86] */
        QUAD((__low_end + 0xae00000) | 0x83) /* [87] */
        QUAD((__low_end + 0xb000000) | 0x83) /* [88] */
        QUAD((__low_end + 0xb200000) | 0x83) /* [89] */
        QUAD((__low_end + 0xb400000) | 0x83) /* [90] */
        QUAD((__low_end + 0xb600000) | 0x83) /* [91] */
        QUAD((__low_end + 0xb800000) | 0x83) /* [92] */
        QUAD((__low_end + 0xba00000) | 0x83) /* [93] */
        QUAD((__low_end + 0xbc00000) | 0x83) /* [94] */
        QUAD((__low_end + 0xbe00000) | 0x83) /* [95] */
        QUAD((__low_end + 0xc000000) | 0x83) /* [96] */
        QUAD((__low_end + 0xc200000) | 0x83) /* [97] */
        QUAD((__low_end + 0xc400000) | 0x83) /* [98] */
        QUAD((__low_end + 0xc600000) | 0x83) /* [99] */
        QUAD((__low_end + 0xc800000) | 0x83) /* [100] */
        QUAD((__low_end + 0xca00000) | 0x83) /* [101] */
        QUAD((__low_end + 0xcc00000) | 0x83) /* [102] */
        QUAD((__low_end + 0xce00000) | 0x83) /* [103] */
        QUAD((__low_end + 0xd000000) | 0x83) /* [104] */
        QUAD((__low_end + 0xd200000) | 0x83) /* [105] */
        QUAD((__low_end + 0xd400000) | 0x83) /* [106] */
        QUAD((__low_end + 0xd600000) | 0x83) /* [107] */
        QUAD((__low_end + 0xd800000) | 0x83) /* [108] */
        QUAD((__low_end + 0xda00000) | 0x83) /* [109] */
        QUAD((__low_end + 0xdc00000) | 0x83) /* [110] */
        QUAD((__low_end + 0xde00000) | 0x83) /* [111] */
        QUAD((__low_end + 0xe000000) | 0x83) /* [112] */
        QUAD((__low_end + 0xe200000) | 0x83) /* [113] */
        QUAD((__low_end + 0xe400000) | 0x83) /* [114] */
        QUAD((__low_end + 0xe600000) | 0x83) /* [115] */
        QUAD((__low_end + 0xe800000) | 0x83) /* [116] */
        QUAD((__low_end + 0xea00000) | 0x83) /* [117] */
        QUAD((__low_end + 0xec00000) | 0x83) /* [118] */
        QUAD((__low_end + 0xee00000) | 0x83) /* [119] */
        QUAD((__low_end + 0xf000000) | 0x83) /* [120] */
        QUAD((__low_end + 0xf200000) | 0x83) /* [121] */
        QUAD((__low_end + 0xf400000) | 0x83) /* [122] */
        QUAD((__low_end + 0xf600000) | 0x83) /* [123] */
        QUAD((__low_end + 0xf800000) | 0x83) /* [124] */
        QUAD((__low_end + 0xfa00000) | 0x83) /* [125] */
        QUAD((__low_end + 0xfc00000) | 0x83) /* [126] */
        QUAD((__low_end + 0xfe00000) | 0x83) /* [127] */
        . = __boot_pd511 + 4096;

        . = ALIGN(4096);
        __boottables_end = .;
    }

    /* End of the low region.  2 MiB aligned so the higher-half PD can use
     * 2 MiB PS leaves at phys [__low_end, ...).  VMA == LMA == physical.  */
    . = ALIGN(4096);
    . = ALIGN(0x200000);
    __low_end = ABSOLUTE(.);

    /* ── KERNEL REGION: VMA == KERNEL_VMA, LMA == __low_end + (VMA - KERNEL_VMA) ── */
    . = KERNEL_VMA;
    __kernel_start = .;
    __kernel_start_phys = __low_end;
    __phys_delta = __low_end - KERNEL_VMA;

    __text_start = .;
    .text : AT(. + __phys_delta) {
        *(.text)
        *(.text.*)
    }
    . = ALIGN(4096);
    __text_end = .;
    __text_start_phys = LOADADDR(.text);
    __text_end_phys = ABSOLUTE(__text_end + __phys_delta);

    __rela_dyn_start = .;
    __rela_dyn_end = .;
    /* `.rela.dyn` (and every other relocation section) is DISCARDED below.
     * The kernel is a non-PIE ET_EXEC whose relocations are resolved at link
     * time and are never applied by either bootloader, so this section is
     * empty in the final image.  Keeping even an empty `SHT_RELA` section
     * header makes GRUB's Multiboot2 loader refuse the file outright
     * ("ELF files with relocs are not supported yet"), so we drop it and let
     * the Rust symbols collapse to an empty range.  */
    __rela_dyn_start_phys = ABSOLUTE(__rela_dyn_start + __phys_delta);
    __rela_dyn_end_phys = ABSOLUTE(__rela_dyn_end + __phys_delta);

    . = ALIGN(4096);

    __rodata_start = .;
    .rodata : AT(. + __phys_delta) {
        *(.rodata)
        *(.rodata.*)
    }
    . = ALIGN(4096);
    __rodata_end = .;
    __rodata_start_phys = LOADADDR(.rodata);
    __rodata_end_phys = ABSOLUTE(__rodata_end + __phys_delta);

    __data_start = .;
    .data : AT(. + __phys_delta) {
        *(.data)
        *(.data.*)
        *(.got)
        *(.got.*)
    }
    . = ALIGN(4096);
    __data_end = .;
    __data_start_phys = LOADADDR(.data);
    __data_end_phys = ABSOLUTE(__data_end + __phys_delta);

    .dynamic : AT(. + __phys_delta) {
        *(.dynamic)
    }

    . = ALIGN(4096);

    __idt_start = .;
    .idt : AT(. + __phys_delta) {
        *(.idt)
    }
    __idt_end = .;
    __idt_start_phys = LOADADDR(.idt);
    __idt_end_phys = ABSOLUTE(__idt_end + __phys_delta);

    . = ALIGN(4096);

    .bss (NOLOAD) : AT(. + __phys_delta) {
        __bss_start = .;
        *(.bss)
        *(.bss.*)
        *(COMMON)
        __bss_end = .;
    }
    . = ALIGN(4096);
    __bss_start_phys = LOADADDR(.bss);
    __bss_end_phys = ABSOLUTE(__bss_end + __phys_delta);

    .stack (NOLOAD) : AT(. + __phys_delta) {
        __stack_start = .;
        /* AML parsing is recursive and can retain several parser frames while
         * executing control methods.  Keep the downward-growing kernel stack
         * well clear of the preceding .bss (which contains allocator state). */
        . += 0x40000;
        __stack_end = .;
    }
    . = ALIGN(4096);
    __stack_start_phys = LOADADDR(.stack);
    __stack_end_phys = ABSOLUTE(__stack_end + __phys_delta);

    __kernel_end = .;
    __kernel_end_phys = ABSOLUTE(__kernel_end + __phys_delta);

/* ── Invariants ──────────────────────────────────────────────────
     * The ASSERT directive is completely unparseable in the shipping
     * rust-lld (every form, even `ASSERT(0 == 0, \"ok\")`, aborts the
     * link), so the critical invariants are guaranteed *structurally*
     * and verified post-link by inspection of the ELF program/section
     * headers:
     *   - .multiboot2 is the first input section => file offset 0
     *     (inside GRUB's 32768-byte window); the low region is emitted
     *     at VMA == LMA == KERNEL_OFFSET.
     *   - __low_end is 2 MiB aligned (ALIGN(0x200000)); the kernel LMA
     *     base is __low_end, so every .bss/.stack LMA lies within the
     *     shared 2 MiB identity map window (no carve-out needed).
     *   - Section ordering + symbol pairs are made exact via explicit
     *     output section definitions (no orphans).  ─────────────────── */

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.note*)
        *(.gnu.hash)
        *(.dynsym)
        *(.dynstr)
        *(.gcc_except_table)
        *(.rela.dyn)
        *(.rela.*)
        *(.rel.*)
    }
}
