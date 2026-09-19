//! The application descriptor the chip's bootloader insists on.
//!
//! **Written out here rather than taken from a crate**, because the two crates
//! that would supply it disagree about where it goes: `esp-bootloader-esp-idf`
//! 0.2 emits `.rodata_desc.appdesc`, and the `esp-hal` 1.2 linker script
//! places `.flash.appdesc`. A descriptor in the wrong section links fine,
//! flashes fine, and then the ROM bootloader reports "Failed to fetch app
//! description header" and refuses to boot -- so the section is the whole
//! point, and pinning it here is more honest than pinning two crate versions
//! that happen to agree.
//!
//! The layout is ESP-IDF's `esp_app_desc_t`: 256 bytes, and it must be the
//! first thing in the image after the header.

/// `ESP_APP_DESC_MAGIC_WORD`.
const MAGIC: u32 = 0xABCD_5432;

#[repr(C)]
pub struct AppDescriptor {
    magic_word: u32,
    secure_version: u32,
    reserved_1: [u32; 2],
    version: [u8; 32],
    project_name: [u8; 32],
    time: [u8; 16],
    date: [u8; 16],
    idf_version: [u8; 32],
    app_elf_sha256: [u8; 32],
    min_efuse_block_revision: u16,
    max_efuse_block_revision: u16,
    mmu_page_size: u8,
    reserved_3: [u8; 3],
    reserved_2: [u32; 18],
}

// The bootloader reads a fixed number of bytes, so the size is part of the
// contract rather than an implementation detail.
const _: () = assert!(core::mem::size_of::<AppDescriptor>() == 256);

/// Copy a string into a fixed, NUL-padded field.
const fn field<const N: usize>(text: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let bytes = text.as_bytes();
    let mut index = 0;
    // One short of N, so the field is always NUL-terminated.
    while index < bytes.len() && index < N - 1 {
        out[index] = bytes[index];
        index += 1;
    }
    out
}

#[used]
#[unsafe(export_name = "esp_app_desc")]
#[unsafe(link_section = ".flash.appdesc")]
pub static APP_DESCRIPTOR: AppDescriptor = AppDescriptor {
    magic_word: MAGIC,
    secure_version: 0,
    reserved_1: [0; 2],
    version: field(env!("CARGO_PKG_VERSION")),
    project_name: field(env!("CARGO_PKG_NAME")),
    time: field("00:00:00"),
    date: field("Jan  1 2026"),
    idf_version: field("bare-metal"),
    // Filled in by the flashing tool if it wants to; the bootloader does not
    // check it.
    app_elf_sha256: [0; 32],
    min_efuse_block_revision: 0,
    max_efuse_block_revision: u16::MAX,
    // log2(64 KB), the C6's flash MMU page size.
    mmu_page_size: 16,
    reserved_3: [0; 3],
    reserved_2: [0; 18],
};
