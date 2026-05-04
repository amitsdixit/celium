//! Loads the CelHyper kernel image from the EFI System Partition and
//! relocates it into a freshly-allocated, page-aligned region of memory.

use alloc::vec::Vec;
use core::ptr::NonNull;
use uefi::CString16;
use uefi::boot::{self, AllocateType};
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::file::{File, FileAttribute, FileMode, RegularFile};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::table::boot::MemoryType;

const KERNEL_PATH: &str = "\\EFI\\CELIUM\\CELHYPER.ELF";

/// A loaded CelHyper image (raw file bytes from the ESP).
pub struct LoadedKernel {
    /// Raw image bytes.
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub enum LoadError {
    NoLoadedImage,
    NoFilesystem,
    NotFound,
    Truncated,
    NotElf,
    BadElf,
    AllocFailed,
    Io,
}

pub fn load_celhyper() -> Result<LoadedKernel, LoadError> {
    let li_handle = boot::image_handle();
    let loaded_image = boot::open_protocol_exclusive::<LoadedImage>(li_handle)
        .map_err(|_| LoadError::NoLoadedImage)?;
    let device = loaded_image.device().ok_or(LoadError::NoLoadedImage)?;

    let mut sfs = boot::open_protocol_exclusive::<SimpleFileSystem>(device)
        .map_err(|_| LoadError::NoFilesystem)?;
    let mut root = sfs.open_volume().map_err(|_| LoadError::NoFilesystem)?;

    let path = CString16::try_from(KERNEL_PATH).map_err(|_| LoadError::NotFound)?;
    let handle = root
        .open(&path, FileMode::Read, FileAttribute::empty())
        .map_err(|_| LoadError::NotFound)?;
    let mut file: RegularFile = handle.into_regular_file().ok_or(LoadError::NotFound)?;

    let mut bytes: Vec<u8> = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|_| LoadError::Io)?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
    }

    if bytes.len() < 64 {
        return Err(LoadError::Truncated);
    }
    if &bytes[0..4] != b"\x7FELF" {
        return Err(LoadError::NotElf);
    }

    Ok(LoadedKernel { bytes })
}

#[must_use]
pub fn parse_entry_point(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < 0x20 {
        return None;
    }
    if &bytes[0..4] != b"\x7FELF" {
        return None;
    }
    if bytes[4] != 2 || bytes[5] != 1 {
        return None;
    }
    let mut e = [0u8; 8];
    e.copy_from_slice(&bytes[0x18..0x20]);
    Some(u64::from_le_bytes(e))
}

/// A fully loaded image in memory: PT_LOAD segments copied to their target
/// VAs (plus a load slide), R_X86_64_RELATIVE relocations applied.
pub struct RelocatedImage {
    pub entry: u64,
    pub base: u64,
    pub size: u64,
}

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Parse `bytes` as ELF64 LE, allocate UEFI pages, copy PT_LOAD segments,
/// apply R_X86_64_RELATIVE relocs, and return the absolute entry point.
pub fn load_and_relocate(bytes: &[u8]) -> Result<RelocatedImage, LoadError> {
    if bytes.len() < 0x40 || &bytes[0..4] != b"\x7FELF" {
        return Err(LoadError::NotElf);
    }
    if bytes[4] != 2 || bytes[5] != 1 {
        return Err(LoadError::BadElf);
    }

    let e_entry = rd_u64(bytes, 0x18);
    let e_phoff = rd_u64(bytes, 0x20) as usize;
    let e_phentsize = rd_u16(bytes, 0x36) as usize;
    let e_phnum = rd_u16(bytes, 0x38) as usize;
    if e_phentsize < 0x38 {
        return Err(LoadError::BadElf);
    }
    if e_phoff + e_phnum * e_phentsize > bytes.len() {
        return Err(LoadError::BadElf);
    }

    // First pass: vmin/vmax across PT_LOAD segments.
    let mut vmin = u64::MAX;
    let mut vmax = 0u64;
    for i in 0..e_phnum {
        let o = e_phoff + i * e_phentsize;
        let p_type = rd_u32(bytes, o);
        if p_type != 1 {
            continue;
        }
        let p_vaddr = rd_u64(bytes, o + 0x10);
        let p_memsz = rd_u64(bytes, o + 0x28);
        if p_vaddr < vmin {
            vmin = p_vaddr;
        }
        if p_vaddr + p_memsz > vmax {
            vmax = p_vaddr + p_memsz;
        }
    }
    if vmax == 0 || vmin == u64::MAX {
        return Err(LoadError::BadElf);
    }

    let span = (vmax - vmin) as usize;
    let pages = (span + 0xFFF) >> 12;
    let nn: NonNull<u8> =
        boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_CODE, pages)
            .map_err(|_| LoadError::AllocFailed)?;
    let region = nn.as_ptr();
    // SAFETY: `pages*0x1000` bytes are owned by us.
    unsafe {
        core::ptr::write_bytes(region, 0, pages * 0x1000);
    }
    let base = region as u64;
    let load_slide = base.wrapping_sub(vmin);

    // Second pass: copy PT_LOAD bytes.
    for i in 0..e_phnum {
        let o = e_phoff + i * e_phentsize;
        let p_type = rd_u32(bytes, o);
        if p_type != 1 {
            continue;
        }
        let p_offset = rd_u64(bytes, o + 0x08) as usize;
        let p_vaddr = rd_u64(bytes, o + 0x10);
        let p_filesz = rd_u64(bytes, o + 0x20) as usize;
        if p_offset + p_filesz > bytes.len() {
            return Err(LoadError::BadElf);
        }
        let dst = (load_slide + p_vaddr) as *mut u8;
        // SAFETY: dst is inside our allocation; ranges checked above.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr().add(p_offset), dst, p_filesz);
        }
    }

    // Third pass: find PT_DYNAMIC and apply R_X86_64_RELATIVE.
    for i in 0..e_phnum {
        let o = e_phoff + i * e_phentsize;
        let p_type = rd_u32(bytes, o);
        if p_type != 2 {
            continue;
        }
        let dyn_off = rd_u64(bytes, o + 0x08) as usize;
        let dyn_sz = rd_u64(bytes, o + 0x20) as usize;

        let mut rela_addr = 0u64;
        let mut rela_size = 0u64;
        let mut rela_ent = 24u64;
        let mut k = 0usize;
        while k + 16 <= dyn_sz {
            let tag = rd_u64(bytes, dyn_off + k) as i64;
            let val = rd_u64(bytes, dyn_off + k + 8);
            match tag {
                0 => break,
                7 => rela_addr = val,
                8 => rela_size = val,
                9 => rela_ent = val,
                _ => {}
            }
            k += 16;
        }
        if rela_size == 0 {
            continue;
        }
        let count = (rela_size / rela_ent) as usize;
        let table_base = (load_slide + rela_addr) as *const u8;
        for j in 0..count {
            // SAFETY: rela table is inside our copied PT_LOAD region.
            let entry = unsafe { table_base.add(j * rela_ent as usize) };
            let r_offset = unsafe { core::ptr::read_unaligned(entry as *const u64) };
            let r_info = unsafe { core::ptr::read_unaligned(entry.add(8) as *const u64) };
            let r_addend = unsafe { core::ptr::read_unaligned(entry.add(16) as *const i64) };
            let r_type = (r_info & 0xFFFF_FFFF) as u32;
            if r_type == 8 {
                let target = (load_slide + r_offset) as *mut u64;
                let value = (load_slide as i64).wrapping_add(r_addend) as u64;
                // SAFETY: target inside our allocation (PT_LOAD coverage).
                unsafe {
                    core::ptr::write_unaligned(target, value);
                }
            }
        }
    }

    Ok(RelocatedImage {
        entry: load_slide + e_entry,
        base,
        size: span as u64,
    })
}
